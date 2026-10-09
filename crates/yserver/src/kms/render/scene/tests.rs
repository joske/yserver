use super::*;

fn audit_rect(x: i32, y: i32, width: u32, height: u32) -> vk::Rect2D {
    vk::Rect2D {
        offset: vk::Offset2D { x, y },
        extent: vk::Extent2D { width, height },
    }
}

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

// Stage 5 Phase G — strategy decision unit tests. Verify the
// pure `derive_cursor_transition` matrix without needing
// build_scene or a live Vk fixture.

#[test]
fn derive_sw_to_hw_queues_show_on_retire() {
    let prev = OutputCursorMode::Sw {
        prev: Some((100, 100)),
    };
    let assignment = CursorAssignment::Hw {
        x: 200,
        y: 150,
        record_version: 42,
        hot_x: 4,
        hot_y: 4,
    };
    let (trans, prev_pos, mode_after) = derive_cursor_transition(prev, assignment);
    let Some(CursorTransition::ShowOnRetire {
        upload_version,
        x,
        y,
        ..
    }) = trans
    else {
        panic!("expected ShowOnRetire, got {trans:?}");
    };
    assert_eq!(upload_version, 42);
    assert_eq!((x, y), (200, 150));
    assert_eq!(prev_pos, Some(None), "Sw→Hw clears prev_pos");
    assert_eq!(mode_after, OutputCursorMode::Hw);
}

#[test]
fn actual_hw_visibility_forces_cursorless_fallback_but_not_hw_rebind() {
    let desired_sw = CursorAssignment::Sw { pos: (100, 100) };
    let sw_prev = effective_cursor_prev_mode(OutputCursorMode::Hidden, true, desired_sw);
    assert_eq!(sw_prev, OutputCursorMode::Hw);
    assert!(cursorless_hide_frame_required(sw_prev, desired_sw));
    let (transition, _, _) = derive_cursor_transition(sw_prev, desired_sw);
    assert!(matches!(
        transition,
        Some(CursorTransition::HideOnRetire {
            reveal_sw_after: true
        })
    ));

    let desired_hidden = CursorAssignment::Hidden;
    let hidden_prev = effective_cursor_prev_mode(
        OutputCursorMode::Sw {
            prev: Some((10, 20)),
        },
        true,
        desired_hidden,
    );
    assert_eq!(hidden_prev, OutputCursorMode::Hw);
    let (transition, _, _) = derive_cursor_transition(hidden_prev, desired_hidden);
    assert!(matches!(
        transition,
        Some(CursorTransition::HideOnRetire {
            reveal_sw_after: false
        })
    ));

    let desired_hw = CursorAssignment::Hw {
        x: 200,
        y: 150,
        record_version: 42,
        hot_x: 4,
        hot_y: 5,
    };
    let hw_prev = effective_cursor_prev_mode(OutputCursorMode::Hidden, true, desired_hw);
    assert_eq!(
        hw_prev,
        OutputCursorMode::Hidden,
        "an old visible binding must not suppress a lifecycle/version Show"
    );
    let (transition, _, _) = derive_cursor_transition(hw_prev, desired_hw);
    assert!(matches!(
        transition,
        Some(CursorTransition::ShowOnRetire {
            upload_version: 42,
            ..
        })
    ));

    let unchanged = OutputCursorMode::Sw { prev: None };
    assert_eq!(
        effective_cursor_prev_mode(unchanged, false, desired_hidden),
        unchanged
    );

    let hw_now_hidden_to_sw = effective_cursor_prev_mode(OutputCursorMode::Hw, false, desired_sw);
    assert_eq!(hw_now_hidden_to_sw, OutputCursorMode::Hidden);
    let (transition, _, mode) = derive_cursor_transition(hw_now_hidden_to_sw, desired_sw);
    assert!(transition.is_none());
    assert!(matches!(mode, OutputCursorMode::Sw { .. }));

    let hw_now_hidden_to_hidden =
        effective_cursor_prev_mode(OutputCursorMode::Hw, false, desired_hidden);
    assert_eq!(hw_now_hidden_to_hidden, OutputCursorMode::Hidden);
    let (transition, _, mode) = derive_cursor_transition(hw_now_hidden_to_hidden, desired_hidden);
    assert!(transition.is_none());
    assert_eq!(mode, OutputCursorMode::Hidden);

    let hw_now_hidden_to_hw = effective_cursor_prev_mode(OutputCursorMode::Hw, false, desired_hw);
    assert_eq!(hw_now_hidden_to_hw, OutputCursorMode::Hidden);
    let (transition, _, _) = derive_cursor_transition(hw_now_hidden_to_hw, desired_hw);
    assert!(matches!(
        transition,
        Some(CursorTransition::ShowOnRetire {
            upload_version: 42,
            ..
        })
    ));
}

#[test]
fn live_source_device_lost_is_the_only_fatal_vulkan_present_error() {
    assert!(vk_result_is_device_lost(vk::Result::ERROR_DEVICE_LOST));
    assert!(!vk_result_is_device_lost(
        vk::Result::ERROR_OUT_OF_DEVICE_MEMORY,
    ));
    assert!(present_error_is_device_lost(&PresentError::Vk(
        vk::Result::ERROR_DEVICE_LOST,
    )));
    assert!(!present_error_is_device_lost(&PresentError::Vk(
        vk::Result::ERROR_OUT_OF_DEVICE_MEMORY,
    )));
}

#[test]
fn copied_renderer_acquire_failure_is_fail_stop_before_submit() {
    let acquire = CopiedRenderSubmitError::RendererAcquire(io::Error::other(
        "retained B-to-A completion import failed",
    ));
    assert!(acquire.requires_fail_stop());
    assert!(matches!(acquire.into_present(), PresentError::Io(_)));

    let ordinary =
        CopiedRenderSubmitError::Present(PresentError::Vk(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY));
    assert!(!ordinary.requires_fail_stop());
}

#[test]
fn upload_failure_hides_only_a_recorded_live_binding() {
    use std::cell::Cell;

    let hidden_hide_calls = Cell::new(0);
    let hidden = resolve_failed_cursor_upload(0, false, || {
        hidden_hide_calls.set(hidden_hide_calls.get() + 1);
        Err(io::Error::from_raw_os_error(libc::EINVAL))
    });
    assert_eq!(hidden, CursorTransitionResult::Hidden);
    assert_eq!(hidden_hide_calls.get(), 0);

    let visible_hide_calls = Cell::new(0);
    let hidden_after_rollback = resolve_failed_cursor_upload(0, true, || {
        visible_hide_calls.set(visible_hide_calls.get() + 1);
        Ok(())
    });
    assert_eq!(hidden_after_rollback, CursorTransitionResult::Hidden);
    assert_eq!(visible_hide_calls.get(), 1);

    let visible_hide_calls = Cell::new(0);
    let retained = resolve_failed_cursor_upload(0, true, || {
        visible_hide_calls.set(visible_hide_calls.get() + 1);
        Err(io::Error::from_raw_os_error(libc::EIO))
    });
    assert_eq!(retained, CursorTransitionResult::VisibleNeedsShowRetry);
    assert_eq!(visible_hide_calls.get(), 1);
}

#[test]
fn retire_hide_skips_ioctl_when_fast_path_already_unbound_the_plane() {
    use std::cell::Cell;

    let calls = Cell::new(0);
    let reveal = resolve_cursor_hide_on_retire(0, true, false, || {
        calls.set(calls.get() + 1);
        Err(io::Error::from_raw_os_error(libc::EINVAL))
    });
    assert_eq!(reveal, CursorTransitionResult::HiddenNeedsRepaint);
    assert_eq!(calls.get(), 0);

    let hidden = resolve_cursor_hide_on_retire(0, false, false, || {
        calls.set(calls.get() + 1);
        Err(io::Error::from_raw_os_error(libc::EINVAL))
    });
    assert_eq!(hidden, CursorTransitionResult::Applied);
    assert_eq!(calls.get(), 0);

    let visible = resolve_cursor_hide_on_retire(0, true, true, || {
        calls.set(calls.get() + 1);
        Err(io::Error::from_raw_os_error(libc::EIO))
    });
    assert_eq!(visible, CursorTransitionResult::Visible);
    assert_eq!(calls.get(), 1);
}

#[test]
fn actual_visible_hw_plus_desired_sw_omits_every_software_cursor_artifact() {
    let cursor_id = crate::kms::render::store::DrawableId::for_tests(100);
    let assignment = CursorAssignment::Sw { pos: (10, 20) };
    let prev = effective_cursor_prev_mode(OutputCursorMode::Hidden, true, assignment);
    let mut built = SceneBuild {
        scene: CompositeScene {
            bg_color: [0.0, 0.0, 0.0, 1.0],
            draws: vec![CompositeDraw {
                image_view: vk::ImageView::null(),
                dst_origin: [10.0, 20.0],
                dst_size: [16.0, 16.0],
                src_origin: [0.0, 0.0],
                src_size: [1.0, 1.0],
                alpha_passthrough: true,
            }],
        },
        snapshots: Vec::new(),
        carried: Vec::new(),
        sampled_ids: vec![cursor_id],
        presented_ids: vec![cursor_id],
        pieces_ids: vec![cursor_id],
        stats: WalkStats::default(),
        projected_damage: RegionSet::new(),
        cursor_assignment: assignment,
        new_cursor_rect: Some(rect(10, 20, 16, 16)),
        cursor_record_version: Some(7),
        software_cursor_tail: Some((0, 0)),
        participants: Vec::new(),
    };
    assert!(cursorless_hide_frame_required(prev, assignment));
    built.omit_software_cursor_for_hide();
    let (transition, _, _) = derive_cursor_transition(prev, assignment);

    assert!(matches!(
        transition,
        Some(CursorTransition::HideOnRetire {
            reveal_sw_after: true
        })
    ));
    assert!(built.scene.draws.is_empty());
    assert!(built.sampled_ids.is_empty());
    assert_eq!(built.new_cursor_rect, None);
    assert_eq!(built.cursor_record_version, None);
}

#[test]
fn hw_to_sw_uses_cursorless_hide_then_one_frame_gap_progression() {
    let assignment = CursorAssignment::Sw { pos: (100, 100) };
    assert!(cursorless_hide_frame_required(
        OutputCursorMode::Hw,
        assignment
    ));
    let (trans, prev_pos, mode_after) = derive_cursor_transition(OutputCursorMode::Hw, assignment);
    assert!(matches!(
        trans,
        Some(CursorTransition::HideOnRetire {
            reveal_sw_after: true
        })
    ));
    assert_eq!(prev_pos, Some(None));
    assert_eq!(mode_after, OutputCursorMode::SwPending);

    let pending =
        resolve_retired_cursor_state(CursorTransitionResult::HiddenNeedsRepaint, mode_after);
    assert_eq!(pending.actual_mode, OutputCursorMode::SwPending);
    assert!(pending.commit_desired_metadata);
    assert!(pending.force_repaint);
    assert!(!cursorless_hide_frame_required(
        pending.actual_mode,
        assignment
    ));
    let (next_transition, next_prev, next_mode) =
        derive_cursor_transition(pending.actual_mode, assignment);
    assert!(next_transition.is_none());
    assert_eq!(next_prev, Some(Some((100, 100))));
    assert_eq!(
        next_mode,
        OutputCursorMode::Sw {
            prev: Some((100, 100))
        }
    );
    assert!(!matches!(next_mode, OutputCursorMode::SwPending));
}

#[test]
fn cursorless_hide_phase_removes_sw_draw_sample_and_presented_metadata() {
    let cursor_id = crate::kms::render::store::DrawableId::for_tests(99);
    let mut built = SceneBuild {
        scene: CompositeScene {
            bg_color: [0.0, 0.0, 0.0, 1.0],
            draws: vec![CompositeDraw {
                image_view: vk::ImageView::null(),
                dst_origin: [10.0, 20.0],
                dst_size: [16.0, 16.0],
                src_origin: [0.0, 0.0],
                src_size: [1.0, 1.0],
                alpha_passthrough: true,
            }],
        },
        snapshots: Vec::new(),
        carried: Vec::new(),
        sampled_ids: vec![cursor_id],
        presented_ids: vec![cursor_id],
        pieces_ids: vec![cursor_id],
        stats: WalkStats::default(),
        projected_damage: RegionSet::new(),
        cursor_assignment: CursorAssignment::Sw { pos: (10, 20) },
        new_cursor_rect: Some(rect(10, 20, 16, 16)),
        cursor_record_version: Some(7),
        software_cursor_tail: Some((0, 0)),
        participants: Vec::new(),
    };

    built.omit_software_cursor_for_hide();

    assert!(built.scene.draws.is_empty());
    assert!(built.sampled_ids.is_empty());
    assert_eq!(built.new_cursor_rect, None);
    assert_eq!(built.cursor_record_version, None);
    assert_eq!(
        built.cursor_assignment,
        CursorAssignment::Sw { pos: (10, 20) }
    );
}

#[test]
fn repeated_hide_failure_keeps_hw_over_cursorless_frames_and_retries() {
    let assignment = CursorAssignment::Sw { pos: (100, 100) };
    let failed =
        resolve_retired_cursor_state(CursorTransitionResult::Visible, OutputCursorMode::SwPending);
    assert_eq!(failed.actual_mode, OutputCursorMode::Hw);
    assert!(failed.force_repaint);
    assert!(cursorless_hide_frame_required(
        failed.actual_mode,
        assignment
    ));
    let (retry, _, mode_after_retry) = derive_cursor_transition(failed.actual_mode, assignment);
    assert!(matches!(
        retry,
        Some(CursorTransition::HideOnRetire {
            reveal_sw_after: true
        })
    ));
    assert_eq!(mode_after_retry, OutputCursorMode::SwPending);
}

#[test]
fn derive_hw_to_hidden_queues_hide_on_retire() {
    let (trans, _prev_pos, mode_after) =
        derive_cursor_transition(OutputCursorMode::Hw, CursorAssignment::Hidden);
    assert!(matches!(
        trans,
        Some(CursorTransition::HideOnRetire {
            reveal_sw_after: false
        })
    ));
    assert_eq!(mode_after, OutputCursorMode::Hidden);
}

#[test]
fn stationary_cursor_same_rect_mode_and_version_adds_no_damage() {
    let damage = cursor_damage_for_frame(
        Some(rect(10, 20, 16, 16)),
        Some(7),
        Some(rect(10, 20, 16, 16)),
        Some(7),
        None,
    );
    assert!(
        damage.is_empty(),
        "stationary cursor must not keep the output dirty"
    );
}

#[test]
fn moved_sw_cursor_damages_old_and_new_rects() {
    let old = rect(10, 20, 16, 16);
    let new = rect(30, 40, 16, 16);
    let damage = cursor_damage_for_frame(Some(old), Some(7), Some(new), Some(7), None);
    let rects = damage.rects();
    assert!(rects.contains(&old), "old rect must be cleared");
    assert!(rects.contains(&new), "new rect must be painted");
}

#[test]
fn sprite_swap_on_stationary_cursor_damages_once() {
    let rect = rect(10, 20, 16, 16);
    let damage = cursor_damage_for_frame(Some(rect), Some(7), Some(rect), Some(8), None);
    assert_eq!(damage.rects(), &[rect]);
}

#[test]
fn pure_hw_hide_still_damages_last_present_rect() {
    let rect = rect(10, 20, 16, 16);
    let damage = cursor_damage_for_frame(
        Some(rect),
        Some(7),
        None,
        None,
        Some(CursorTransition::HideOnRetire {
            reveal_sw_after: false,
        }),
    );
    assert_eq!(damage.rects(), &[rect]);
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

/// Steady-state HW ordinarily queues no transition. A sprite/hotspot
/// change separately sets `force_show_retry`, which converts the next
/// composed Hw→Hw frame into ShowOnRetire.
#[test]
fn derive_hw_to_hw_no_transition() {
    let assignment = CursorAssignment::Hw {
        x: 0,
        y: 0,
        record_version: 7,
        hot_x: 0,
        hot_y: 0,
    };
    let (trans, _prev_pos, mode_after) = derive_cursor_transition(OutputCursorMode::Hw, assignment);
    assert!(trans.is_none());
    assert_eq!(mode_after, OutputCursorMode::Hw);
}

#[test]
fn pending_show_output_receives_later_sprite_retry() {
    let pending_show = Some(CursorTransition::ShowOnRetire {
        upload_version: 7,
        hot_x: 0,
        hot_y: 0,
        x: 10,
        y: 20,
    });

    assert!(cursor_output_needs_sprite_retry(
        OutputCursorMode::Sw { prev: None },
        [pending_show]
    ));
    assert!(cursor_output_needs_sprite_retry(
        OutputCursorMode::Hw,
        [None]
    ));
    assert!(!cursor_output_needs_sprite_retry(
        OutputCursorMode::Hidden,
        [None]
    ));
}

#[test]
fn failed_unbound_show_records_hidden_and_clears_claimed_metadata() {
    let resolution =
        resolve_retired_cursor_state(CursorTransitionResult::Hidden, OutputCursorMode::Hw);
    assert_eq!(resolution.actual_mode, OutputCursorMode::Hidden);
    assert!(!resolution.commit_desired_metadata);
    assert!(resolution.clear_presented_metadata);
}

#[test]
fn failed_hide_retains_actual_hw_and_last_known_metadata() {
    let resolution = resolve_retired_cursor_state(
        CursorTransitionResult::Visible,
        OutputCursorMode::Sw {
            prev: Some((10, 20)),
        },
    );
    assert_eq!(resolution.actual_mode, OutputCursorMode::Hw);
    assert!(!resolution.commit_desired_metadata);
    assert!(!resolution.clear_presented_metadata);
}

#[test]
fn failed_visible_rebind_forces_full_show_retry_without_claiming_version() {
    let resolution = resolve_retired_cursor_state(
        CursorTransitionResult::VisibleNeedsShowRetry,
        OutputCursorMode::Hw,
    );
    assert_eq!(resolution.actual_mode, OutputCursorMode::Hw);
    assert!(!resolution.commit_desired_metadata);
    assert!(!resolution.clear_presented_metadata);
    let retry = update_force_show_retry_version(
        None,
        Some(CursorTransition::ShowOnRetire {
            upload_version: 7,
            hot_x: 0,
            hot_y: 0,
            x: 10,
            y: 20,
        }),
        CursorTransitionResult::VisibleNeedsShowRetry,
        OutputCursorMode::Hw,
    );
    assert_eq!(retry, Some(7));
}

#[test]
fn unrelated_old_ack_cannot_clear_newer_show_retry() {
    assert_eq!(
        update_force_show_retry_version(
            Some(8),
            None,
            CursorTransitionResult::Applied,
            OutputCursorMode::Hw,
        ),
        Some(8)
    );
}

#[test]
fn older_show_retirement_cannot_clear_newer_sprite_retry() {
    let old_show = Some(CursorTransition::ShowOnRetire {
        upload_version: 8,
        hot_x: 0,
        hot_y: 0,
        x: 10,
        y: 20,
    });
    assert_eq!(
        update_force_show_retry_version(
            Some(9),
            old_show,
            CursorTransitionResult::Applied,
            OutputCursorMode::Hw,
        ),
        Some(9)
    );
}

#[test]
fn matching_show_success_clears_only_its_retry_generation() {
    let show = Some(CursorTransition::ShowOnRetire {
        upload_version: 9,
        hot_x: 0,
        hot_y: 0,
        x: 10,
        y: 20,
    });
    assert_eq!(
        update_force_show_retry_version(
            Some(9),
            show,
            CursorTransitionResult::Applied,
            OutputCursorMode::Hw,
        ),
        None
    );
}

#[test]
fn lifecycle_reset_discards_stale_retry_before_fresh_hidden_show() {
    let mut retry = Some(1);
    reset_cursor_retry_for_lifecycle(&mut retry);
    assert_eq!(retry, None);
    let mut mode = OutputCursorMode::SwPending;
    reset_cursor_mode_for_lifecycle(&mut mode);
    assert_eq!(mode, OutputCursorMode::Hidden);

    let fresh_show = Some(CursorTransition::ShowOnRetire {
        upload_version: 2,
        hot_x: 0,
        hot_y: 0,
        x: 10,
        y: 20,
    });
    retry = update_force_show_retry_version(
        retry,
        fresh_show,
        CursorTransitionResult::Applied,
        OutputCursorMode::Hw,
    );
    assert_eq!(retry, None);
}

/// Sw → Sw and Hidden → Sw produce no transition (no plane
/// state change) but the mode advances to Sw so the next
/// frame's derivation sees the right "prev".
#[test]
fn derive_sw_or_hidden_to_sw_advances_mode_no_transition() {
    let (trans, _prev_pos, mode_after) = derive_cursor_transition(
        OutputCursorMode::Sw { prev: None },
        CursorAssignment::Sw { pos: (50, 50) },
    );
    assert!(trans.is_none());
    assert!(matches!(mode_after, OutputCursorMode::Sw { .. }));

    let (trans, _, mode_after) = derive_cursor_transition(
        OutputCursorMode::Hidden,
        CursorAssignment::Sw { pos: (50, 50) },
    );
    assert!(trans.is_none());
    assert!(matches!(mode_after, OutputCursorMode::Sw { .. }));
}

/// Dual-output regression: cursor on monitor 1 only (output 0 =
/// Hw, output 1 = Hidden) MUST classify as `Hw`, not `Mixed`.
/// Pre-fix this returned Mixed and routed every motion event
/// through scene.wake_for_damage — the HW cursor never moved on
/// silence.
#[test]
fn classify_cursor_mode_dual_output_cursor_on_one_monitor_is_hw() {
    let modes = [OutputCursorMode::Hw, OutputCursorMode::Hidden];
    assert_eq!(
        classify_cursor_mode_from_per_output(modes),
        CursorPlaneMode::Hw,
    );
    // Order shouldn't matter.
    let modes = [OutputCursorMode::Hidden, OutputCursorMode::Hw];
    assert_eq!(
        classify_cursor_mode_from_per_output(modes),
        CursorPlaneMode::Hw,
    );
}

/// Single-output Hw is Hw; single-output Hidden is Sw (degenerate
/// — no Hw plane active, scene wake is what'd update a future SW
/// cursor draw).
#[test]
fn classify_cursor_mode_single_output_cases() {
    assert_eq!(
        classify_cursor_mode_from_per_output([OutputCursorMode::Hw]),
        CursorPlaneMode::Hw,
    );
    assert_eq!(
        classify_cursor_mode_from_per_output([OutputCursorMode::Hidden]),
        CursorPlaneMode::Sw,
    );
    assert_eq!(
        classify_cursor_mode_from_per_output([OutputCursorMode::Sw { prev: None }]),
        CursorPlaneMode::Sw,
    );
    assert_eq!(
        classify_cursor_mode_from_per_output([OutputCursorMode::SwPending]),
        CursorPlaneMode::Sw,
    );
}

/// Hw + Sw on different outputs IS Mixed (one output's SW sprite
/// is in the compose draw list; the other's plane is bound). The
/// fast path must defer until the SW output transitions out, or
/// the plane could desync from the SW sprite position.
#[test]
fn classify_cursor_mode_hw_and_sw_is_mixed() {
    let modes = [OutputCursorMode::Hw, OutputCursorMode::Sw { prev: None }];
    assert_eq!(
        classify_cursor_mode_from_per_output(modes),
        CursorPlaneMode::Mixed,
    );

    let pending_reveal = [OutputCursorMode::Hw, OutputCursorMode::SwPending];
    assert_eq!(
        classify_cursor_mode_from_per_output(pending_reveal),
        CursorPlaneMode::Mixed,
        "a cursorless hide gap must remain non-direct until SW actually retires"
    );
}

/// Empty input degenerates to Sw (no outputs = no Hw plane to
/// drive; the fast path has nothing to optimise anyway).
#[test]
fn classify_cursor_mode_no_outputs_is_sw() {
    let empty: [OutputCursorMode; 0] = [];
    assert_eq!(
        classify_cursor_mode_from_per_output(empty),
        CursorPlaneMode::Sw,
    );
}

/// `cursor_mode()` returns `Mixed` while any output's PendingAck
/// carries an unretired cursor transition — the load-bearing
/// query gate for the pointer fast path.
#[test]
fn cursor_mode_mixed_when_transition_pending() {
    let mut scene = SceneCompositor::stub();
    // Stub has `inner == None`; cursor_mode collapses to Sw.
    assert_eq!(scene.cursor_mode(), CursorPlaneMode::Sw);
    // The pending-transition path can only be triggered with
    // a real inner; covered by integration smoke + the
    // separate `derive_*` tests above.
    let _ = &mut scene;
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

/// Stage 4c.1 — code-quality follow-up. The plural setter test
/// above only proves the dirty bit gets set on the stub-mode
/// compositor (where `inner` is `None` and the dispatch for-loop
/// is unreachable). `clip_rect_to_output_extent_handles_all_cases`
/// only covers the helper math in isolation. Their union does
/// NOT cover the dispatch wiring — a regression that swapped
/// `damage.add(clipped)` for a no-op (or dropped the per-output
/// loop entirely) would pass both tests. This test exercises
/// the extracted `dispatch_clip_rects_to_outputs` helper that
/// `mark_scene_structure_damage_rects` delegates to, with two
/// synthetic outputs of different extents, and asserts that:
///
/// - a rect wholly inside lands unchanged on every output;
/// - a rect spilling off the right edge lands clipped (NOT in
///   its original form) on the output where it spills;
/// - a rect that's fully outside an output is dropped for that
///   output but still lands on the other output if it fits there;
/// - per-output clipping is independent (extent of output A does
///   not influence what lands on output B).
#[test]
fn dispatch_clip_rects_lands_per_output_clipped() {
    // Output 0: 800×600, Output 1: 400×400. Same input rect set.
    let ext_a = extent(800, 600);
    let ext_b = extent(400, 400);
    let mut damage_a = RegionSet::new();
    let mut damage_b = RegionSet::new();

    let inside = rect(10, 20, 100, 50); // fits both outputs
    let spilling_right = rect(700, 0, 200, 50); // spills A on right; fully outside B
    let outside_a_inside_b = rect(350, 350, 30, 30); // fits both (B clips to 50×50)
    let fully_outside = rect(2000, 2000, 50, 50); // outside both

    let rects = [inside, spilling_right, outside_a_inside_b, fully_outside];

    // Build a `Vec` of tuples so the slice carries a stable lifetime for
    // the iterator; the production callsite produces
    // `(origin, extent, &mut damage)`. Both outputs sit at the origin here,
    // which is the case where root-absolute and output-local coincide — see
    // the test below for the case where they do not.
    let mut outs: Vec<((i32, i32), vk::Extent2D, &mut RegionSet)> = vec![
        ((0, 0), ext_a, &mut damage_a),
        ((0, 0), ext_b, &mut damage_b),
    ];
    dispatch_clip_rects_to_outputs(outs.drain(..), &rects);

    // Output A (800×600):
    //   - inside (10,20,100,50): identity
    //   - spilling_right (700,0,200,50): clipped width 200→100
    //   - outside_a_inside_b (350,350,30,30): identity (fits A)
    //   - fully_outside: dropped
    let a_rects = damage_a.rects();
    assert!(
        a_rects.contains(&inside),
        "inside rect must land unchanged on output A: {a_rects:?}",
    );
    let spilling_clipped_a = rect(700, 0, 100, 50);
    assert!(
        a_rects.contains(&spilling_clipped_a),
        "spilling rect must land CLIPPED on output A (expected {spilling_clipped_a:?}), got {a_rects:?}",
    );
    assert!(
        !a_rects.contains(&spilling_right),
        "spilling rect must NOT land in its original (unclipped) form on output A: {a_rects:?}",
    );
    assert!(
        a_rects.contains(&outside_a_inside_b),
        "rect that fits output A unchanged must land: {a_rects:?}",
    );
    assert!(
        !a_rects.iter().any(|r| r.offset.x >= 800
            || r.offset.y >= 600
            || i64::from(r.offset.x) + i64::from(r.extent.width) > 800
            || i64::from(r.offset.y) + i64::from(r.extent.height) > 600),
        "no rect on output A may spill its 800×600 extent: {a_rects:?}",
    );

    // Output B (400×400):
    //   - inside (10,20,100,50): identity
    //   - spilling_right (700,0,...): fully outside → dropped
    //   - outside_a_inside_b (350,350,30,30): clipped → (350,350,30,30) fits in 400×400
    //   - fully_outside: dropped
    let b_rects = damage_b.rects();
    assert!(
        b_rects.contains(&inside),
        "inside rect must land unchanged on output B: {b_rects:?}",
    );
    assert!(
        !b_rects.iter().any(|r| r.offset.x >= 700),
        "spilling-right (x=700) is fully outside output B and must be dropped: {b_rects:?}",
    );
    assert!(
        b_rects.contains(&outside_a_inside_b),
        "rect that fits output B unchanged must land: {b_rects:?}",
    );
    assert!(
        !b_rects
            .iter()
            .any(|r| i64::from(r.offset.x) + i64::from(r.extent.width) > 400
                || i64::from(r.offset.y) + i64::from(r.extent.height) > 400),
        "no rect on output B may spill its 400×400 extent: {b_rects:?}",
    );
}

/// Stage 4c.1 — the helper clips a rect to the output's extent
/// (offset assumed (0,0) — output-local coords). Wholly inside
/// → identity. Partially overlapping → clipped intersection.
/// Fully outside or zero-area → `None`.
#[test]
fn clip_rect_to_output_extent_handles_all_cases() {
    let ext = extent(800, 600);

    // Wholly inside — identity.
    assert_eq!(
        clip_rect_to_output_extent(rect(10, 20, 100, 50), ext),
        Some(rect(10, 20, 100, 50)),
    );

    // Right edge spills — clip width.
    assert_eq!(
        clip_rect_to_output_extent(rect(700, 0, 200, 50), ext),
        Some(rect(700, 0, 100, 50)),
    );

    // Bottom edge spills — clip height.
    assert_eq!(
        clip_rect_to_output_extent(rect(0, 500, 50, 200), ext),
        Some(rect(0, 500, 50, 100)),
    );

    // Negative offset — clamp to 0, clip width.
    assert_eq!(
        clip_rect_to_output_extent(rect(-30, -20, 100, 80), ext),
        Some(rect(0, 0, 70, 60)),
    );

    // Wholly to the right — None.
    assert_eq!(clip_rect_to_output_extent(rect(900, 0, 50, 50), ext), None);

    // Wholly below — None.
    assert_eq!(clip_rect_to_output_extent(rect(0, 700, 50, 50), ext), None);

    // Zero-width — None.
    assert_eq!(clip_rect_to_output_extent(rect(10, 10, 0, 50), ext), None);

    // Zero-height — None.
    assert_eq!(clip_rect_to_output_extent(rect(10, 10, 50, 0), ext), None);
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

fn extent(w: u32, h: u32) -> vk::Extent2D {
    vk::Extent2D {
        width: w,
        height: h,
    }
}

#[test]
fn buffer_age_ring_trims_to_depth() {
    let mut ring = BufferAgeRing::new(3);
    for g in 1..=5 {
        let mut r = RegionSet::new();
        r.add(rect(0, 0, 4, 4));
        ring.push(g, r);
    }
    assert_eq!(ring.entries.len(), 3);
    // Oldest entries trimmed: 1, 2 gone; 3, 4, 5 remain.
    let gens: Vec<u64> = ring.entries.iter().map(|(g, _)| *g).collect();
    assert_eq!(gens, vec![3, 4, 5]);
}

#[test]
fn buffer_age_contains_all_strict_window() {
    let mut ring = BufferAgeRing::new(4);
    let mut r = RegionSet::new();
    r.add(rect(0, 0, 4, 4));
    ring.push(3, r.clone());
    ring.push(4, r.clone());
    // BO last_gen=2, frame_gen=5 → intervening gens 3, 4.
    assert!(ring.contains_all(2, 5));
    // BO last_gen=2, frame_gen=6 → needs 3, 4, 5 — 5 missing.
    assert!(!ring.contains_all(2, 6));
    // No intervening gens (frame_gen == last_gen+1).
    assert!(ring.contains_all(2, 3));
}

// ── Step 4: the gates that make clipping safe ─────────────────

fn draw_at(x: f32, y: f32, w: f32, h: f32, alpha_passthrough: bool) -> CompositeDraw {
    CompositeDraw {
        image_view: vk::ImageView::null(),
        dst_origin: [x, y],
        dst_size: [w, h],
        src_origin: [0.0, 0.0],
        src_size: [1.0, 1.0],
        alpha_passthrough,
    }
}

/// An opaque full-output bottom layer, i.e. what the root draw is.
fn opaque_root(w: f32, h: f32) -> CompositeDraw {
    draw_at(0.0, 0.0, w, h, false)
}

fn region_of(rects: &[vk::Rect2D]) -> Region {
    Region::from_rects(rects.iter().copied())
}

fn small_damage() -> Region {
    region_of(&[audit_rect(10, 10, 40, 40)])
}

#[test]
fn clipped_path_is_taken_for_small_damage_under_an_opaque_root() {
    let draws = [opaque_root(800.0, 600.0)];
    let plan = plan_repaint(&small_damage(), &draws, extent(800, 600), true, true);
    assert!(plan.full_reason.is_none());
    let Repaint::Clipped(rect) = plan.repaint else {
        panic!("expected Clipped, got {:?}", plan.repaint);
    };
    assert_eq!(rect, audit_rect(10, 10, 40, 40));
    // `painted` is what the recorder will cover: the bbox.
    assert_eq!(plan.painted.bounding_rect(), Some(rect));
}

#[test]
fn painted_always_covers_what_was_requested() {
    // The invariant `commit_submitted` asserts. Checked here for every gate
    // outcome, because a Full fallback must also claim the whole output.
    let cases: [(&[CompositeDraw], bool, bool); 4] = [
        (&[opaque_root(800.0, 600.0)], true, true),
        (&[], true, true),
        (&[opaque_root(800.0, 600.0)], false, true),
        (&[draw_at(0.0, 0.0, 800.0, 600.0, true)], true, true),
    ];
    for (draws, loadable, shared) in cases {
        let requested = small_damage();
        let plan = plan_repaint(&requested, draws, extent(800, 600), loadable, shared);
        assert!(
            plan.painted.contains(&requested),
            "painted must cover requested for {:?}",
            plan.full_reason
        );
    }
}

#[test]
fn empty_draw_list_forces_full() {
    let plan = plan_repaint(&small_damage(), &[], extent(800, 600), true, true);
    assert_eq!(plan.full_reason, Some(FullReason::EmptyDrawList));
    assert!(matches!(plan.repaint, Repaint::Full(_)));
}

#[test]
fn unloadable_bo_forces_full() {
    let draws = [opaque_root(800.0, 600.0)];
    let plan = plan_repaint(&small_damage(), &draws, extent(800, 600), false, true);
    assert_eq!(plan.full_reason, Some(FullReason::UnloadableBo));
}

#[test]
fn copied_route_forces_full() {
    let draws = [opaque_root(800.0, 600.0)];
    let plan = plan_repaint(&small_damage(), &draws, extent(800, 600), true, false);
    assert_eq!(plan.full_reason, Some(FullReason::CopiedRoute));
}

#[test]
fn a_blended_bottom_layer_forces_full() {
    // Every COW-subtree draw is alpha_passthrough by construction, so a
    // compositing desktop lands here — correctly, and at no cost, since a
    // compositor presents a full-screen surface every frame anyway.
    let draws = [draw_at(0.0, 0.0, 800.0, 600.0, true)];
    let plan = plan_repaint(&small_damage(), &draws, extent(800, 600), true, true);
    assert_eq!(plan.full_reason, Some(FullReason::NoOpaqueCover));
}

#[test]
fn an_opaque_draw_that_does_not_reach_the_damage_forces_full() {
    // Opaque, but only over part of the output: the uncovered part of the
    // region would show whatever the previous compose of this BO left.
    let draws = [draw_at(0.0, 0.0, 20.0, 20.0, false)];
    let plan = plan_repaint(&small_damage(), &draws, extent(800, 600), true, true);
    assert_eq!(plan.full_reason, Some(FullReason::NoOpaqueCover));
}

#[test]
fn a_fractional_edge_does_not_count_as_covering() {
    // dst is f32; rounding inward means a half-pixel short of the damage is
    // not cover. The guard can only ever be conservative.
    let requested = region_of(&[audit_rect(0, 0, 800, 600)]);
    let draws = [draw_at(0.5, 0.0, 800.0, 600.0, false)];
    assert!(!opaque_cover_exists(
        &draws,
        requested.bounding_rect().expect("non-empty")
    ));
}

#[test]
fn damage_above_the_threshold_renders_full() {
    // Below the threshold clips, above it does not; the constant is the only
    // thing that moves between these two.
    let draws = [opaque_root(800.0, 600.0)];
    let below = region_of(&[audit_rect(0, 0, 800, 300)]); // 0.5
    assert!(
        plan_repaint(&below, &draws, extent(800, 600), true, true)
            .full_reason
            .is_none()
    );
    let above = region_of(&[audit_rect(0, 0, 800, 420)]); // 0.7
    assert_eq!(
        plan_repaint(&above, &draws, extent(800, 600), true, true).full_reason,
        Some(FullReason::Threshold)
    );
}

#[test]
fn the_threshold_is_measured_on_what_will_be_painted() {
    // Superseded the earlier "measured on the bounding box" rule when 4.5
    // landed: the box is only what gets painted when the frame renders under
    // a single scissor. Two small rects at opposite corners have a near-full
    // box and a tiny area — under 4.5 they render per rect and must stay
    // clipped, because the box is never rasterised.
    let draws = [opaque_root(800.0, 600.0)];
    let sparse = region_of(&[audit_rect(0, 0, 8, 8), audit_rect(790, 590, 8, 8)]);
    assert!(sparse.area() < 200, "region really is tiny");
    let plan = plan_repaint(&sparse, &draws, extent(800, 600), true, true);
    assert!(plan.full_reason.is_none(), "per-rect keeps this clipped");
    assert_eq!(plan.scissors.len(), 2);

    // The converse — one big scissor being measured on its own area — is
    // pinned by `damage_above_the_threshold_renders_full`.
}

// ── 4.5: per-rect rendering ──────────────────────────────────

#[test]
fn a_single_contiguous_damage_rect_renders_under_one_scissor() {
    let draws = [opaque_root(800.0, 600.0)];
    let plan = plan_repaint(&small_damage(), &draws, extent(800, 600), true, true);
    assert_eq!(plan.scissors.len(), 1);
    assert_eq!(plan.scissors[0], audit_rect(10, 10, 40, 40));
}

#[test]
fn two_separated_rects_render_per_rect_and_painted_excludes_the_gap() {
    // The drag shape: a window's old and new positions. The bounding box
    // spans both plus the empty gap, which measured 36% waste on hardware.
    let draws = [opaque_root(800.0, 600.0)];
    let dragged = region_of(&[audit_rect(0, 0, 100, 100), audit_rect(300, 0, 100, 100)]);
    let plan = plan_repaint(&dragged, &draws, extent(800, 600), true, true);
    assert!(plan.full_reason.is_none());
    assert_eq!(plan.scissors.len(), 2, "should render per rect");
    assert_eq!(plan.painted.area(), 2 * 100 * 100, "the gap is not painted");
    // `repaint` stays the bbox: it is the render area, not the scissor.
    assert!(matches!(plan.repaint, Repaint::Clipped(_)));
}

#[test]
fn adjacent_rects_stay_under_one_scissor() {
    // Touching rects coalesce in the Region, so there is no gap to save and
    // no reason to pay for a second pass.
    let draws = [opaque_root(800.0, 600.0)];
    let touching = region_of(&[audit_rect(0, 0, 50, 50), audit_rect(50, 0, 50, 50)]);
    let plan = plan_repaint(&touching, &draws, extent(800, 600), true, true);
    assert_eq!(plan.scissors.len(), 1);
}

#[test]
fn per_rect_rendering_keeps_frames_off_the_full_path() {
    // Two rects whose bounding box is over the threshold but whose actual
    // area is well under it. Thresholding on the box would render Full and
    // paint the whole screen; thresholding on what will be painted clips.
    let draws = [opaque_root(800.0, 600.0)];
    let spread = region_of(&[audit_rect(0, 0, 100, 100), audit_rect(700, 500, 100, 100)]);
    let bbox_fraction = 800.0 * 600.0 / (800.0 * 600.0);
    assert!(
        bbox_fraction >= CLIPPED_REPAINT_MAX_FRACTION,
        "bbox is the screen"
    );
    let plan = plan_repaint(&spread, &draws, extent(800, 600), true, true);
    assert!(
        plan.full_reason.is_none(),
        "per-rect should have kept this clipped, got {:?}",
        plan.full_reason
    );
    assert_eq!(plan.painted.area(), 2 * 100 * 100);
}

#[test]
fn a_fragmented_region_still_renders_per_rect() {
    // Superseded "too many rects falls back to the box", which pinned the
    // 8-rect cap. That cap made 4.5 stop engaging on MATE, where panels and
    // the desktop fragment a drag region past 8 — reintroducing the 34% box
    // waste the step exists to remove. The bound that matters is the
    // region's own rect cap, and the draw-call cost is scissors × the
    // POST-cull draw count, which is ~4.
    let draws = [opaque_root(800.0, 600.0)];
    let mut scattered = Region::new();
    for i in 0..12 {
        scattered.add_rect(audit_rect(i * 60, i * 40, 10, 10));
    }
    assert_eq!(scattered.rect_count(), 12);
    let plan = plan_repaint(&scattered, &draws, extent(800, 600), true, true);
    assert_eq!(
        plan.scissors.len(),
        12,
        "each fragment gets its own scissor"
    );
    assert_eq!(
        plan.painted.area(),
        12 * 100,
        "and the gaps are not painted"
    );
}

#[test]
fn a_region_past_its_own_cap_arrives_already_collapsed() {
    // `Region` collapses to its extents above MAX_RECTS, so `plan_repaint`
    // can never see an unbounded list — which is why the scissor cap no
    // longer needs to bind.
    let draws = [opaque_root(800.0, 600.0)];
    let mut many = Region::new();
    for i in 0..(Region::MAX_RECTS + 10) {
        many.add_rect(audit_rect((i as i32 % 40) * 20, (i as i32 / 40) * 20, 5, 5));
    }
    assert!(many.rect_count() <= Region::MAX_RECTS);
    let plan = plan_repaint(&many, &draws, extent(800, 600), true, true);
    assert!(plan.scissors.len() <= Region::MAX_RECTS);
}

#[test]
fn scissors_are_disjoint_which_the_overlay_xor_depends_on() {
    // The root IncludeInferiors overlay is not idempotent, so each of its
    // pixels must fall in exactly ONE scissor. Region rects are disjoint by
    // construction; this pins it, including across a band boundary, which is
    // where a hand-rolled rect list would overlap.
    let draws = [opaque_root(800.0, 600.0)];
    let straddling = region_of(&[
        audit_rect(0, 0, 100, 30),
        audit_rect(0, 30, 40, 30),
        audit_rect(300, 0, 100, 100),
    ]);
    let plan = plan_repaint(&straddling, &draws, extent(800, 600), true, true);
    let s = &plan.scissors;
    for (i, a) in s.iter().enumerate() {
        for b in &s[i + 1..] {
            assert!(!rects_intersect(*a, *b), "scissors overlap: {a:?} {b:?}");
        }
    }
    // And they cover exactly the damage, no more.
    let mut covered = Region::new();
    for r in s {
        covered.add_rect(*r);
    }
    if s.len() > 1 {
        assert_eq!(covered, straddling);
    }
}

#[test]
fn a_full_fallback_still_claims_the_whole_output() {
    let plan = plan_repaint(&small_damage(), &[], extent(800, 600), true, true);
    assert_eq!(plan.scissors, vec![audit_rect(0, 0, 800, 600)]);
    assert_eq!(plan.painted.area(), 800 * 600);
}

#[test]
fn culling_drops_draws_outside_the_rect_and_keeps_order() {
    let scene = CompositeScene {
        bg_color: [0.0, 0.0, 0.0, 1.0],
        draws: vec![
            opaque_root(800.0, 600.0),
            draw_at(400.0, 400.0, 50.0, 50.0, true),
            draw_at(10.0, 10.0, 20.0, 20.0, true),
        ],
    };
    let culled = cull_scene_to_region(&scene, &Region::from_rect(audit_rect(0, 0, 100, 100)));
    assert_eq!(culled.draws.len(), 2, "the far draw is culled");
    assert_eq!(culled.draws[0].dst_size, [800.0, 600.0], "root stays first");
    assert_eq!(culled.draws[1].dst_origin, [10.0, 10.0]);
    assert_eq!(culled.bg_color, scene.bg_color);
    // The guard the clipped path depends on must survive its own cull.
    assert!(opaque_cover_exists(
        &culled.draws,
        audit_rect(0, 0, 100, 100)
    ));
}

// ── Stage 3f.6: subwindow scene traversal ─────────────────────

fn alloc_stub_window(
    store: &mut DrawableStore,
    windows: &mut crate::kms::render::backend::WindowsMap,
    xid: u32,
    x: i16,
    y: i16,
    w: u16,
    h: u16,
    parent: Option<u32>,
    mapped: bool,
) {
    // for_tests_null gives null image handles; build_scene
    // rejects null views. Use a non-zero sentinel handle so the
    // traversal test exercises the recurse logic. The handle
    // never gets passed to Vk because the test never composes.
    let mut storage = crate::kms::render::store::Storage::for_tests_null(
        extent(u32::from(w), u32::from(h)),
        vk::Format::B8G8R8A8_UNORM,
    );
    // SAFETY: Vk handle types are opaque u64s; constructing a
    // sentinel doesn't touch the driver. The `is_test_stub`
    // flag on Storage means Drop won't try to destroy these.
    // Stamp both views to the same sentinel so build_scene's
    // sample-side bind (`storage.sample_view`) sees the same
    // handle the legacy tests asserted against — these stubs
    // don't exercise α swizzle, just storage-routing.
    let sentinel: ash::vk::ImageView = ash::vk::Handle::from_raw(u64::from(xid) | 0xFF00_0000);
    storage.image_view = sentinel;
    storage.sample_view = sentinel;
    store
        .allocate(xid, DrawableKind::Window, 32, mapped, storage)
        .expect("stub allocate");
    windows.insert(
        xid,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x,
            y,
            width: w,
            height: h,
            depth: 32,
            mapped,
            viewable: true,
            parent,
            stack_rank: 0,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );
}

/// Stage 3f.6 — `build_scene` walks top-level → mapped
/// descendants and produces draw entries in absolute coords.
/// Top-level at (50, 60), child at (10, 20) relative → child
/// emits at output coords (60, 80).
#[test]
fn build_scene_recurses_into_mapped_children() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    // Top-level @ (50, 60), 200×100.
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x100,
        50,
        60,
        200,
        100,
        None,
        true,
    );
    core.top_level_order.push(0x100);

    // Child @ (10, 20) relative to top-level, 40×30.
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x101,
        10,
        20,
        40,
        30,
        Some(0x100),
        true,
    );

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        None,
        false,
        Visibility::Off,
    );
    let scene = built.scene;
    assert_eq!(scene.draws.len(), 2, "expected top-level + child draw");

    // Top-level at output (50, 60) since output layout origin is (0,0).
    let top = scene
        .draws
        .iter()
        .find(|d| d.dst_size[0] == 200.0 && d.dst_size[1] == 100.0)
        .expect("top-level draw present");
    assert_eq!(top.dst_origin, [50.0, 60.0]);

    // Child at absolute (60, 80) = top (50, 60) + child rel (10, 20).
    let child = scene
        .draws
        .iter()
        .find(|d| d.dst_size[0] == 40.0 && d.dst_size[1] == 30.0)
        .expect("child draw present");
    assert_eq!(child.dst_origin, [60.0, 80.0]);
}

/// X11 parent-clipping: a child window is clipped to its parent's
/// rectangle. fvwm (and other WMs) park oversized frame-decoration
/// windows in a tiny off-screen holding window so they're invisible;
/// yserver must not paint the whole child. Regression: fvwm's 1146×23
/// title bar, parked in a 10×10 holding window at (-10,-10), leaked a
/// ~1136×13 white strip onto the top-left of the screen because the
/// scene drew the child at full size (air/silence HW 2026-07-02).
#[test]
fn build_scene_clips_child_to_parent_bounds() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    // Small parent @ (100, 100), 10×10 (the holding window).
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x100,
        100,
        100,
        10,
        10,
        None,
        true,
    );
    core.top_level_order.push(0x100);

    // Oversized child @ (2, 3) relative, 100×50 — far larger than the
    // 10×10 parent. Only the intersection with the parent may show.
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x101,
        2,
        3,
        100,
        50,
        Some(0x100),
        true,
    );

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        None,
        false,
        Visibility::Off,
    );
    let scene = built.scene;

    // The child draw must be clipped to the parent's rect, NOT the
    // full 100×50. Child abs (102,103) ∩ parent (100,100,110,110)
    // = (102,103)-(110,110) → 8×7.
    let child = scene
        .draws
        .iter()
        .find(|d| d.dst_origin == [102.0, 103.0])
        .expect("child draw present at its absolute origin");
    assert_eq!(
        child.dst_size,
        [8.0, 7.0],
        "child must be clipped to the parent's 10×10 bounds, not drawn \
             at full 100×50 (parent-clipping); got {:?}",
        child.dst_size,
    );
    assert!(
        (child.src_size[0] - 8.0 / 100.0).abs() < 1e-5
            && (child.src_size[1] - 7.0 / 50.0).abs() < 1e-5,
        "src_size must sample only the visible sub-region, got {:?}",
        child.src_size,
    );
    // No draw may exceed the parent's footprint.
    assert!(
        !scene
            .draws
            .iter()
            .any(|d| d.dst_size[0] > 10.0 || d.dst_size[1] > 10.0),
        "no draw may exceed the 10×10 parent, got {:?}",
        scene.draws,
    );
}

/// SHAPE bounding region clips the window's scene draw. Marco
/// uses `SHAPE-Request: Rectangles destination=Bounding` to set
/// a rounded-corner mask on frame windows; without honouring it
/// the scene paints the full rectangle and shows the scanout
/// clear colour (black) in the corners instead of the layer
/// below — diagnosed 2026-05-30 on non-composited MATE.
#[test]
fn build_scene_clips_window_to_shape_bounding() {
    use yserver_protocol::x11::xfixes::RegionRect;
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    // Top-level @ (50, 60), 200×100.
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x100,
        50,
        60,
        200,
        100,
        None,
        true,
    );
    core.top_level_order.push(0x100);

    // Bounding mask: a single sub-rect inset (10, 14) from the
    // window's top-left, 180×80 — analogous to one of marco's
    // rounded-corner approximation strips.
    core.shape_bounding.insert(
        0x100,
        vec![RegionRect {
            x: 10,
            y: 14,
            width: 180,
            height: 80,
        }],
    );

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        None,
        false,
        Visibility::Off,
    );
    let scene = built.scene;

    // Exactly one draw for this window, clipped to the bounding
    // rect — NOT a full-window 200×100 draw.
    let window_draws: Vec<_> = scene
        .draws
        .iter()
        .filter(|d| d.dst_size != [200.0, 100.0])
        .collect();
    assert_eq!(
        window_draws.len(),
        1,
        "expected one draw per bounding rect, got {}: {:?}",
        scene.draws.len(),
        scene.draws,
    );
    let d = window_draws[0];
    // dst: window absolute origin + bounding-rect offset.
    assert_eq!(d.dst_origin, [60.0, 74.0], "dst_origin = (50+10, 60+14)");
    assert_eq!(d.dst_size, [180.0, 80.0], "dst_size = bounding rect");
    // src UV: the sub-region of the window's texture that
    // corresponds to the bounding rect.
    assert!(
        (d.src_origin[0] - 10.0 / 200.0).abs() < 1e-5
            && (d.src_origin[1] - 14.0 / 100.0).abs() < 1e-5,
        "src_origin = (10/200, 14/100), got {:?}",
        d.src_origin,
    );
    assert!(
        (d.src_size[0] - 180.0 / 200.0).abs() < 1e-5 && (d.src_size[1] - 80.0 / 100.0).abs() < 1e-5,
        "src_size = (180/200, 80/100), got {:?}",
        d.src_size,
    );
}

/// DRIFT 1 (findings 2026-06-18), live render half: the empty-vs-
/// absent bounding-shape distinction the Step-1a `Option` API
/// preserves. An EXPLICIT empty bounding region (`Some([])`, stored
/// as an empty Vec) must clip the window to nothing — zero draws —
/// whereas an ABSENT entry renders the full window. Before Step 1a
/// the backend deleted empty rects, collapsing the two so an empty
/// region wrongly rendered as a full window.
#[test]
fn build_scene_empty_bounding_emits_no_draw() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    alloc_stub_window(
        &mut store,
        &mut windows,
        0x100,
        50,
        60,
        200,
        100,
        None,
        true,
    );
    core.top_level_order.push(0x100);
    // Explicit EMPTY bounding region: entry present, zero rects.
    core.shape_bounding.insert(0x100, Vec::new());

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        None,
        false,
        Visibility::Off,
    );
    assert!(
        built.scene.draws.is_empty(),
        "an explicit empty bounding region must emit no draw (window \
             clipped to nothing), got {:?}",
        built.scene.draws,
    );
}

#[test]
fn build_scene_absent_bounding_emits_full_window() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    alloc_stub_window(
        &mut store,
        &mut windows,
        0x100,
        50,
        60,
        200,
        100,
        None,
        true,
    );
    core.top_level_order.push(0x100);
    // No shape_bounding entry at all (absent) → full-window draw.

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        None,
        false,
        Visibility::Off,
    );
    let window_draws: Vec<_> = built
        .scene
        .draws
        .iter()
        .filter(|d| d.dst_size == [200.0, 100.0])
        .collect();
    assert_eq!(
        window_draws.len(),
        1,
        "absent bounding shape must emit one full-window draw, got {:?}",
        built.scene.draws,
    );
    assert_eq!(window_draws[0].dst_origin, [50.0, 60.0]);
}

/// Stage 3f.6 — unmapped parent hides the entire subtree per
/// X11 MapWindow cascade semantics. Child stays scene-
/// participating but doesn't render because its ancestor is
/// unmapped.
#[test]
fn build_scene_unmapped_parent_hides_subtree() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    alloc_stub_window(
        &mut store,
        &mut windows,
        0x200,
        10,
        10,
        100,
        100,
        None,
        false, /* parent NOT mapped */
    );
    core.top_level_order.push(0x200);
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x201,
        0,
        0,
        50,
        50,
        Some(0x200),
        true, /* child IS mapped, but parent isn't */
    );

    let scene = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        None,
        false,
        Visibility::Off,
    )
    .scene;
    assert!(
        scene.draws.is_empty(),
        "unmapped parent must short-circuit subtree (got {} draws)",
        scene.draws.len()
    );
}

/// Stage 3f.8 — when `cursor` is `Some`, `build_scene` emits an
/// additional top-of-z draw entry at the cursor's
/// hot-spot-adjusted position. The entry is the LAST element of
/// `draws` (last = topmost in z-order) and has
/// `alpha_passthrough=true` so the sprite's alpha actually
/// blends.
#[test]
fn build_scene_appends_cursor_draw_at_top_of_z() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    // One mapped top-level so we can verify "cursor is on top".
    alloc_stub_window(&mut store, &mut windows, 0x100, 0, 0, 400, 300, None, true);
    core.top_level_order.push(0x100);

    // Allocate a stub cursor storage entry (synthetic xid).
    let mut storage = crate::kms::render::store::Storage::for_tests_null(
        extent(16, 16),
        vk::Format::B8G8R8A8_UNORM,
    );
    // SAFETY: opaque u64 Vk handle for the cursor's view; the
    // stub Storage's `is_test_stub` flag means Drop won't free
    // it. Stamp both views so scene binds the sample-side.
    let cur_sentinel: ash::vk::ImageView = ash::vk::Handle::from_raw(0xCAFE_BABE);
    storage.image_view = cur_sentinel;
    storage.sample_view = cur_sentinel;
    let cursor_id = store
        .allocate(0xCAFE_0001, DrawableKind::Pixmap, 32, false, storage)
        .expect("alloc cursor stub");

    core.cursor_x = 50.0;
    core.cursor_y = 60.0;
    let cursor = CursorEntry {
        id: cursor_id,
        extent: extent(16, 16),
        hot_x: 0,
        hot_y: 0,
        record_version: 0,
        bgra_bytes: None,
    };

    let scene = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        Some(cursor),
        None,
        None,
        false,
        Visibility::Off,
    )
    .scene;
    // 1 top-level + 1 cursor = 2.
    assert_eq!(scene.draws.len(), 2);
    let cursor_draw = scene.draws.last().expect("cursor draw");
    assert_eq!(cursor_draw.dst_origin, [50.0, 60.0]);
    assert_eq!(cursor_draw.dst_size, [16.0, 16.0]);
    assert!(
        cursor_draw.alpha_passthrough,
        "cursor must blend (sprite has transparent border)"
    );
}

/// Stage 4c.3 / 4c.5 — Automatic-mode invariant.
///
/// When a window W has `redirected_target = Some(B)` AND
/// `scene_participating == true` (Automatic redirect), the scene
/// entry for W blits FROM B's storage (its `image_view`), not
/// from W's own storage. W's geometry (`dst_origin`, `dst_size`)
/// stays driven by `windows[W]`. `sampled_ids` carries B_id
/// (not W_id) so damage/fence accounting follows the source the
/// scene actually read from. B is also marked
/// `scene_participating=true` per Stage 4c's Automatic-mode
/// pairing (the protocol handler issues
/// `set_backing_scene_participation(true)` alongside W's flip).
///
/// 4c.5 rename: framed around the Automatic-mode invariant per
/// task 4c.5 self-review — the assertion shape already matches.
#[test]
fn build_scene_automatic_redirect_keeps_window_via_backing_storage() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    // Window W @ (50, 60), 200×100 — emits at output coords
    // (50, 60) since the test output layout origin is (0, 0).
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x100,
        50,
        60,
        200,
        100,
        None,
        true,
    );
    core.top_level_order.push(0x100);

    // Allocate a separate backing pixmap B with its OWN sentinel
    // image_view, distinct from W's. B is allocated with
    // `scene_participating=false` (Pixmap default) — that's fine
    // for the build-scene path since the resolution looks up
    // storage directly; only the peek for B's damage needs the
    // flag, which we toggle below to verify the snapshot path
    // keys off `source_id`.
    let mut b_storage = crate::kms::render::store::Storage::for_tests_null(
        extent(200, 100),
        vk::Format::B8G8R8A8_UNORM,
    );
    let b_view: vk::ImageView = ash::vk::Handle::from_raw(0xB000_BEEF);
    b_storage.image_view = b_view;
    // Stub both views to the same sentinel — see
    // `alloc_stub_window` for rationale; tests verify
    // routing, not swizzle semantics.
    b_storage.sample_view = b_view;
    let b_id = store
        .allocate(0xB001, DrawableKind::Pixmap, 32, true, b_storage)
        .expect("alloc backing stub");

    // Confirm W and B have distinct image_views.
    let w_id = store.lookup(0x100).expect("w_id present");
    let w_view = store.get(w_id).expect("w drawable").storage.image_view;
    assert_ne!(
        w_view, b_view,
        "fixture sanity: W and B must have distinct sentinel views"
    );

    // Fixture sanity (4c.5 Automatic-mode invariant): W stays
    // scene_participating=true under Automatic redirect; the
    // backing also flips to scene_participating=true (the
    // protocol-side pairing). `alloc_stub_window(mapped=true)`
    // and the `allocate(..., true, _)` above wire both flags.
    assert!(
        store.get(w_id).unwrap().scene_participating,
        "Automatic redirect: W must stay scene_participating=true",
    );
    assert!(
        store.get(b_id).unwrap().scene_participating,
        "Automatic redirect: B must be scene_participating=true",
    );

    // Wire the redirect route: W's source-storage now resolves
    // through B.
    store.set_redirected_target(w_id, Some(b_id));

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        None,
        false,
        Visibility::Off,
    );
    let scene = &built.scene;
    assert_eq!(
        scene.draws.len(),
        1,
        "expected one draw entry for W (geometry unchanged by redirect)"
    );
    let w_draw = &scene.draws[0];

    // Geometry still W's.
    assert_eq!(
        w_draw.dst_origin,
        [50.0, 60.0],
        "redirected W's on-screen rect must remain W's geometry"
    );
    assert_eq!(
        w_draw.dst_size,
        [200.0, 100.0],
        "redirected W's on-screen size must remain W's geometry"
    );

    // Storage handle reroutes to B. The stub fixture stamps
    // both `image_view` and `sample_view` to the same sentinel,
    // so this also implicitly verifies the scene-α fix is
    // binding the sample-side view (no separate handle to
    // distinguish in the stub world — production builds them
    // distinct via `PlatformBackend::build_sample_view`).
    assert_eq!(
        w_draw.image_view, b_view,
        "redirected W must sample FROM B's view, not W's"
    );

    // `sampled_ids` parallels `draws`; the entry for W must
    // carry B_id (the source the scene actually read from) so
    // damage/fence accounting follows the right drawable.
    assert_eq!(built.sampled_ids.len(), 1);
    assert_eq!(
        built.sampled_ids[0], b_id,
        "sampled_ids must carry source_id (B_id) for damage / fence keying"
    );
}

/// Stage 4c.5 — Manual-mode invariant.
///
/// `build_scene`'s `scene_participating` filter (scene.rs:1110 and
/// :922) drops any drawable with `scene_participating == false`
/// from the per-output draw list. Manual-redirected windows carry
/// `scene_participating=false` (the protocol handler issues
/// `set_window_scene_participation(W, false)` on Manual activation)
/// so they MUST NOT appear in `scene.draws` nor in
/// `built.sampled_ids`. Plain unredirected/Automatic windows
/// stay participating and continue to emit.
///
/// Setup: two top-level windows W1 + W2, both mapped and same
/// geometry shape (so the filter is the only thing distinguishing
/// them). W1 stays `scene_participating=true`; W2 is flipped to
/// `false` post-allocation via `set_scene_participating` to
/// mimic the Manual-redirect activation path. The build must
/// emit one draw (W1) and zero entries for W2.
#[test]
fn build_scene_skips_manual_redirected_window() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    // W1 @ (10, 20), 50×40 — Automatic / unredirected
    // (scene_participating=true via `alloc_stub_window`'s
    // `mapped` arg, which the helper forwards as the
    // `scene_participating` flag in `store.allocate`).
    alloc_stub_window(&mut store, &mut windows, 0x111, 10, 20, 50, 40, None, true);
    core.top_level_order.push(0x111);

    // W2 @ (100, 200), 60×30 — geometry that doesn't overlap
    // W1 so a stray draw entry would be unambiguous.
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x222,
        100,
        200,
        60,
        30,
        None,
        true,
    );
    core.top_level_order.push(0x222);

    // Flip W2 off the scene (Manual-redirect activation). Use
    // the store's setter directly — the backend method does
    // more bookkeeping (damage clear + scene-structure damage
    // rect) than this no-Vk scene-walk test needs.
    let w2_id = store.lookup(0x222).expect("w2 lookup");
    store.set_scene_participating(w2_id, false);
    let w1_id = store.lookup(0x111).expect("w1 lookup");
    assert!(
        store.get(w1_id).unwrap().scene_participating,
        "fixture sanity: W1 stays scene_participating=true",
    );
    assert!(
        !store.get(w2_id).unwrap().scene_participating,
        "fixture sanity: W2 must be scene_participating=false",
    );

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        None,
        false,
        Visibility::Off,
    );
    let scene = &built.scene;

    // Only W1's draw entry must be present.
    assert_eq!(
        scene.draws.len(),
        1,
        "Manual-redirected W2 must be filtered from scene.draws (saw {} entries: {:?})",
        scene.draws.len(),
        scene.draws,
    );
    let w1_draw = &scene.draws[0];
    assert_eq!(
        w1_draw.dst_origin,
        [10.0, 20.0],
        "the surviving draw must be W1 (origin (10,20)), NOT W2 (origin (100,200))",
    );
    assert_eq!(
        w1_draw.dst_size,
        [50.0, 40.0],
        "the surviving draw must be W1 (50×40), NOT W2 (60×30)",
    );

    // sampled_ids mirrors draws — must carry W1's id only.
    assert_eq!(built.sampled_ids.len(), 1);
    assert_eq!(
        built.sampled_ids[0], w1_id,
        "sampled_ids must reference W1; W2 was filtered before push",
    );
}

/// Stage 4d — `build_scene` must skip non-redirected descendants
/// of a Manual-redirected ancestor. The descendants' paint
/// routes through `resolve_paint_target` to the ancestor's B;
/// emitting their own (stale) storage on top of the ancestor's B
/// would muddy the compositor output.
///
/// Audit #3 follow-up (2026-05-19): the test was originally
/// written against the degenerate state where the parent has
/// `scene_participating=false` *without* a redirected backing —
/// that state doesn't occur in real life (Manual-redirect
/// activation always sets `redirected_target` BEFORE flipping
/// `scene_participating=false`, see
/// `activate_redirect_backing_for`). Updated to mirror the
/// realistic state: frame has both a backing AND
/// `scene_participating=false`.
///
/// Phase 3.1 update: the parent is Manual-redirected so it ALSO
/// no longer emits (the compositor reads its backing via
/// `NameWindowPixmap` and re-emits it on the COW). The remaining
/// invariant is "the non-redirected child must NOT leak into
/// scene.draws"; the bystander stands in as a positive control.
#[test]
fn build_scene_prunes_descendants_of_manual_redirected_ancestor() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    // Frame W @ (100, 200), 200×150 — the manually-redirected
    // ancestor (CC's marco-decorated frame in production).
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x111,
        100,
        200,
        200,
        150,
        None,
        true,
    );
    core.top_level_order.push(0x111);

    // Child C inside frame W at relative (11, 41), 100×80.
    // scene_participating=true (regular window — only the
    // ancestor is redirected). This is CC's GtkWindow in
    // production: a regular window whose paints route to the
    // frame's redirected backing via resolve_paint_target's
    // ancestor walk.
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x112,
        11,
        41,
        100,
        80,
        Some(0x111),
        true,
    );

    // Bystander top-level W @ (500, 500) so a "did anything
    // get emitted?" assertion isn't ambiguous.
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x222,
        500,
        500,
        60,
        30,
        None,
        true,
    );
    core.top_level_order.push(0x222);

    // Set up realistic Manual-redirect state on frame W: allocate
    // a backing, point W's `redirected_target` at it, then flip
    // `scene_participating=false`. Child stays participating —
    // its paint will resolve to frame_B via
    // `resolve_paint_target`'s ancestor walk, NOT to its own
    // storage; so the child's storage stays stale, and emitting
    // it would muddy the frame_B emit underneath.
    let w_frame_id = store.lookup(0x111).expect("frame lookup");
    let mut frame_backing = crate::kms::render::store::Storage::for_tests_null(
        extent(200, 150),
        vk::Format::B8G8R8A8_UNORM,
    );
    let frame_backing_view: vk::ImageView = ash::vk::Handle::from_raw(0xBEEF_F111);
    frame_backing.image_view = frame_backing_view;
    frame_backing.sample_view = frame_backing_view;
    let frame_backing_id = store
        .allocate(0xB111, DrawableKind::Pixmap, 32, true, frame_backing)
        .expect("alloc frame backing");
    store.set_redirected_target(w_frame_id, Some(frame_backing_id));
    store.set_scene_participating(w_frame_id, false);
    let child_id = store.lookup(0x112).expect("child lookup");
    assert!(
        store.get(child_id).unwrap().scene_participating,
        "fixture sanity: child stays scene_participating=true",
    );

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        None,
        false,
        Visibility::Off,
    );
    let scene = &built.scene;

    // Phase 3.1 — only the bystander emits. The Manual-redirected
    // frame is unconditionally skipped (compositor consumes its
    // backing offscreen via NameWindowPixmap); the non-redirected
    // child must also stay out (its paint resolves to frame_B via
    // the ancestor walk, so emitting its stale storage would muddy
    // the compositor's re-emit on the COW).
    assert_eq!(
        scene.draws.len(),
        1,
        "expected bystander only; got {} — Manual-redirected frame and \
             its non-redirected child must both stay out of scene.draws: {:?}",
        scene.draws.len(),
        scene.draws,
    );
    assert!(
        scene.draws.iter().any(|d| d.dst_origin == [500.0, 500.0]),
        "bystander draw missing: {:?}",
        scene.draws
    );
    // The "must not leak" property — frame backing AND child draw
    // entries must both be absent from scene.draws.
    assert!(
        !scene
            .draws
            .iter()
            .any(|d| d.dst_origin == [100.0, 200.0] && d.dst_size == [200.0, 150.0]),
        "Manual-redirected frame leaked into scene.draws: {:?}",
        scene.draws,
    );
    assert!(
        !scene.draws.iter().any(|d| d.dst_origin == [111.0, 241.0]),
        "non-redirected child of Manual-redirected ancestor leaked into scene.draws: {:?}",
        scene.draws,
    );
    // sampled_ids mirrors draws — bystander only, no frame_B, no child.
    let bystander_id = store.lookup(0x222).expect("bystander lookup");
    assert_eq!(built.sampled_ids.len(), 1);
    assert!(!built.sampled_ids.contains(&frame_backing_id));
    assert!(built.sampled_ids.contains(&bystander_id));
}

// Phase 3.1 — the legacy `build_scene_emits_manual_redirected_parent_backing_but_prunes_descendants`
// test was deleted here. Its sole purpose was to assert that a
// Manual-redirected top-level emits its backing directly into
// scanout — exactly the bug-shaped state Task 3.1 closes. The
// compositor (in production) reads the backing via
// `NameWindowPixmap` and re-emits it on the COW; the X server
// must never short-circuit that. `manual_redirected_top_level_skips_emit_unconditional`
// covers the replacement invariant.

/// Audit #3 (2026-05-19) — a Manual-redirected parent still
/// prunes its NON-redirected descendants (their paint resolves
/// to the parent's B via `resolve_paint_target` so the parent
/// emit covers them), but Automatic-redirected descendants have
/// their OWN backing — `resolve_paint_target` stops at them —
/// and MUST still emit. Pre-fix `prune_subtree=true` dropped
/// them unconditionally, matching the audit's "GTK/marco CSD
/// pattern: RedirectWindow(frame, Manual) +
/// RedirectSubwindows(frame, Automatic) makes Automatic
/// widgets vanish" symptom (Control Center missing menus /
/// widgets).
///
/// Phase 3.1 update: the Manual-redirected parent ALSO no longer
/// emits (compositor reads its backing via NameWindowPixmap).
/// The load-bearing assertion of this test is still "Automatic
/// child backing emits despite Manual ancestor"; the parent emit
/// is dropped from the expectation set.
#[test]
fn build_scene_emits_automatic_descendant_under_manual_ancestor() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    // Frame F at (100, 200), 200×150 — Manual-redirected
    // (scene_participating=false) with its own backing F_B.
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x111,
        100,
        200,
        200,
        150,
        None,
        true,
    );
    core.top_level_order.push(0x111);
    let frame_id = store.lookup(0x111).expect("frame lookup");

    let mut frame_backing = crate::kms::render::store::Storage::for_tests_null(
        extent(200, 150),
        vk::Format::B8G8R8A8_UNORM,
    );
    let frame_backing_view: vk::ImageView = ash::vk::Handle::from_raw(0xBEEF_F000);
    frame_backing.image_view = frame_backing_view;
    frame_backing.sample_view = frame_backing_view;
    let frame_backing_id = store
        .allocate(0xB111, DrawableKind::Pixmap, 32, true, frame_backing)
        .expect("alloc frame backing");
    store.set_redirected_target(frame_id, Some(frame_backing_id));
    store.set_scene_participating(frame_id, false);

    // Automatic-redirected child C at (11, 41) inside F — own
    // backing C_B; scene_participating=true (Automatic).
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x112,
        11,
        41,
        100,
        80,
        Some(0x111),
        true,
    );
    let child_id = store.lookup(0x112).expect("child lookup");

    let mut child_backing = crate::kms::render::store::Storage::for_tests_null(
        extent(100, 80),
        vk::Format::B8G8R8A8_UNORM,
    );
    let child_backing_view: vk::ImageView = ash::vk::Handle::from_raw(0xBEEF_C000);
    child_backing.image_view = child_backing_view;
    child_backing.sample_view = child_backing_view;
    let child_backing_id = store
        .allocate(0xB112, DrawableKind::Pixmap, 32, true, child_backing)
        .expect("alloc child backing");
    store.set_redirected_target(child_id, Some(child_backing_id));
    // Automatic mode → child window stays scene_participating=true.
    assert!(
        store.get(child_id).unwrap().scene_participating,
        "fixture sanity: Automatic-redirected child stays scene_participating=true",
    );

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        None,
        false,
        Visibility::Off,
    );
    let scene = &built.scene;

    // Phase 3.1 — only the Automatic child backing emits, at
    // (111, 241) (= F.pos + C.pos relative). Parent F is
    // Manual-redirected so it stays out of scene.draws; the
    // compositor consumes its backing offscreen via
    // NameWindowPixmap.
    assert_eq!(
        scene.draws.len(),
        1,
        "expected automatic-child backing only (Manual parent skipped); got {:?}",
        scene.draws
    );
    assert!(
        !scene
            .draws
            .iter()
            .any(|d| d.dst_origin == [100.0, 200.0] && d.dst_size == [200.0, 150.0]),
        "Manual parent backing must NOT emit: {:?}",
        scene.draws
    );
    assert!(
        scene
            .draws
            .iter()
            .any(|d| d.dst_origin == [111.0, 241.0] && d.dst_size == [100.0, 80.0]),
        "automatic child backing draw missing: {:?}",
        scene.draws
    );
    assert!(!built.sampled_ids.contains(&frame_backing_id));
    assert!(built.sampled_ids.contains(&child_backing_id));
}

/// Phase 1 pre-cleanup — when no COW is registered
/// (`cow=None`), `build_scene` walks the top-level order and
/// emits a draw entry per mapped top-level. This preserves the
/// legacy non-redirected path that Phase 1 (COW-authoritative)
/// leaves unchanged; the `cow=Some` shape (top-levels stripped)
/// gets its own dedicated test.
#[test]
fn build_scene_cow_none_emits_top_levels() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    // Two mapped top-levels.
    alloc_stub_window(&mut store, &mut windows, 0x100, 0, 0, 100, 80, None, true);
    core.top_level_order.push(0x100);
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x101,
        200,
        150,
        120,
        90,
        None,
        true,
    );
    core.top_level_order.push(0x101);

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None, // no cursor in this fixture
        None,
        None, // cow_host_xid — Phase 2.6 (None = no compositor active)
        false,
        Visibility::Off,
    );
    let scene = &built.scene;

    // Expect: top-level 0x100, top-level 0x101. Two entries
    // total (no cursor, no COW).
    assert_eq!(
        scene.draws.len(),
        2,
        "expected 2 top-levels, got {} draws: {:?}",
        scene.draws.len(),
        scene.draws,
    );

    // Top-level 0x100 at (0, 0) sized 100×80.
    assert_eq!(
        scene.draws[0].dst_origin,
        [0.0, 0.0],
        "first top-level origin",
    );
    assert_eq!(
        scene.draws[0].dst_size,
        [100.0, 80.0],
        "first top-level size",
    );
    // Top-level 0x101 at (200, 150) sized 120×90.
    assert_eq!(
        scene.draws[1].dst_origin,
        [200.0, 150.0],
        "second top-level origin",
    );
    assert_eq!(
        scene.draws[1].dst_size,
        [120.0, 90.0],
        "second top-level size",
    );

    // No draw should be screen-extent (no COW present).
    for d in &scene.draws {
        assert_ne!(
            d.dst_size,
            [800.0, 600.0],
            "no draw should be screen-extent when cow=None: {:?}",
            d,
        );
    }
}

/// Phase 1 pre-cleanup — when no COW is registered
/// (`cow=None`), the cursor draw must still be appended at
/// the top of z above the top-level draws. This preserves the
/// legacy non-redirected cursor-on-top assertion that Phase 1
/// leaves unchanged. The COW-present cursor ordering (top-levels
/// stripped, COW below cursor) gets its own dedicated test.
#[test]
fn build_scene_cow_none_cursor_at_top() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    // One mapped top-level so the scene has anchor content.
    alloc_stub_window(&mut store, &mut windows, 0x100, 0, 0, 400, 300, None, true);
    core.top_level_order.push(0x100);

    // Cursor sprite.
    let mut cursor_storage = crate::kms::render::store::Storage::for_tests_null(
        extent(16, 16),
        vk::Format::B8G8R8A8_UNORM,
    );
    let cur2_sentinel: ash::vk::ImageView = ash::vk::Handle::from_raw(0xCAFE_BABE);
    cursor_storage.image_view = cur2_sentinel;
    cursor_storage.sample_view = cur2_sentinel;
    let cursor_id = store
        .allocate(0xCAFE_0002, DrawableKind::Pixmap, 32, false, cursor_storage)
        .expect("alloc cursor stub");
    core.cursor_x = 50.0;
    core.cursor_y = 60.0;
    let cursor = CursorEntry {
        id: cursor_id,
        extent: extent(16, 16),
        hot_x: 0,
        hot_y: 0,
        record_version: 0,
        bgra_bytes: None,
    };

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        Some(cursor),
        None,
        None, // cow_host_xid — Phase 2.6 (None = no compositor active)
        false,
        Visibility::Off,
    );
    let scene = &built.scene;

    // Expect: top-level, cursor — 2 draws, in that order.
    assert_eq!(
        scene.draws.len(),
        2,
        "expected top-level + cursor = 2 draws, got {}: {:?}",
        scene.draws.len(),
        scene.draws,
    );
    // Last draw = cursor (16×16).
    assert_eq!(
        scene.draws.last().expect("cursor").dst_size,
        [16.0, 16.0],
        "cursor must be the top-of-z draw",
    );
    // First draw = top-level (400×300).
    assert_eq!(
        scene.draws[0].dst_size,
        [400.0, 300.0],
        "top-level must be below cursor",
    );
}

/// Phase 2.6 — `under_cow_subtree` recursion flag propagates
/// `alpha_passthrough = true` to every `CompositeDraw` emitted
/// inside the COW subtree (the COW top-level itself + all of its
/// descendants). Non-COW top-levels (the no-compositor path)
/// emit with `alpha_passthrough = false`.
#[test]
fn cow_subtree_draws_inherit_alpha_passthrough_true() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    // Non-COW top-level W @ (0, 0), 200×200.
    alloc_stub_window(&mut store, &mut windows, 0xA1, 0, 0, 200, 200, None, true);
    core.top_level_order.push(0xA1);

    // COW host xid @ (0, 0), 800×600 — matches PlatformBackend::for_tests output.
    let cow_xid: u32 = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
    alloc_stub_window(
        &mut store,
        &mut windows,
        cow_xid,
        0,
        0,
        800,
        600,
        None,
        true,
    );
    core.top_level_order.push(cow_xid);

    // Compositor stage as child of COW @ (0, 0), 800×600.
    alloc_stub_window(
        &mut store,
        &mut windows,
        0xB1,
        0,
        0,
        800,
        600,
        Some(cow_xid),
        true,
    );

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        Some(cow_xid),
        false,
        Visibility::Off,
    );
    let scene = &built.scene;

    // The non-COW W (200×200) must have alpha_passthrough=false.
    let w_draw = scene
        .draws
        .iter()
        .find(|d| d.dst_size == [200.0, 200.0])
        .expect("W draw present");
    assert!(
        !w_draw.alpha_passthrough,
        "non-COW top-level uses opaque blend (alpha_passthrough=false)",
    );

    // COW + stage (both 800×600) must have alpha_passthrough=true.
    let cow_or_stage_draws: Vec<_> = scene
        .draws
        .iter()
        .filter(|d| d.dst_size == [800.0, 600.0])
        .collect();
    assert!(
        !cow_or_stage_draws.is_empty(),
        "COW and stage emitted: {:?}",
        scene.draws,
    );
    for d in cow_or_stage_draws {
        assert!(
            d.alpha_passthrough,
            "COW subtree draw must have alpha_passthrough=true: {:?}",
            d,
        );
    }
}

/// Phase 2.7 — the COW must emit via the normal `top_level_order`
/// walk, NOT via a special post-walk append. With the COW as the
/// sole top-level, the scene contains exactly one draw sourced
/// from the COW's storage (alpha_passthrough=true from Task 2.6),
/// not two.
#[test]
fn build_scene_does_not_append_cow_after_top_level_walk() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    let cow_xid: u32 = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
    alloc_stub_window(
        &mut store,
        &mut windows,
        cow_xid,
        0,
        0,
        800,
        600,
        None,
        true,
    );
    core.top_level_order.push(cow_xid);

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        Some(cow_xid),
        false,
        Visibility::Off,
    );
    let scene = &built.scene;

    let cow_draws: Vec<_> = scene
        .draws
        .iter()
        .filter(|d| d.dst_size == [800.0, 600.0])
        .collect();
    assert_eq!(
        cow_draws.len(),
        1,
        "exactly one COW draw — no special append on top of top_level_order walk; got {:?}",
        scene.draws,
    );
    assert!(
        cow_draws[0].alpha_passthrough,
        "COW draw still has alpha_passthrough=true",
    );
}

/// Phase 3.1 — a Manual-redirected top-level (own
/// `redirected_target` + `scene_participating=false`) must NEVER
/// emit a `CompositeDraw` from its backing, regardless of whether
/// the COW is materialized. Xorg's `compCheckRedirect` ensures
/// Manual-redirected windows go offscreen for the compositor to
/// read via `NameWindowPixmap`; the X server must not also blit
/// the backing into scanout.
#[test]
fn manual_redirected_top_level_skips_emit_unconditional() {
    for cow_host_xid in [None, Some(0x103_u32)] {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows = crate::kms::render::backend::WindowsMap::new();

        // W with a redirected backing (Manual mode:
        // scene_participating=false). Unique sentinel handle so
        // a stray draw entry is unambiguous.
        let w: u32 = 0xA1;
        alloc_stub_window(&mut store, &mut windows, w, 100, 100, 50, 50, None, true);
        let w_id = store.lookup(w).expect("w lookup");
        let mut backing = crate::kms::render::store::Storage::for_tests_null(
            extent(50, 50),
            PlatformBackend::format_for_depth(24),
        );
        let view: vk::ImageView = ash::vk::Handle::from_raw(0xBEEF_0000);
        backing.image_view = view;
        backing.sample_view = view;
        let b_id = store
            .allocate(0xB0A1, DrawableKind::Pixmap, 24, true, backing)
            .expect("alloc manual backing");
        store.set_redirected_target(w_id, Some(b_id));
        store.set_scene_participating(w_id, false);
        core.top_level_order.push(w);

        if let Some(cow_xid) = cow_host_xid {
            alloc_stub_window(
                &mut store,
                &mut windows,
                cow_xid,
                0,
                0,
                800,
                600,
                None,
                true,
            );
            core.top_level_order.push(cow_xid);
        }

        let built = build_scene(
            &core,
            &mut store,
            &windows,
            0,
            &platform,
            None,
            None,
            cow_host_xid,
            false,
            Visibility::Off,
        );
        let scene = &built.scene;

        let w_draws: Vec<_> = scene
            .draws
            .iter()
            .filter(|d| d.dst_size == [50.0, 50.0])
            .collect();
        assert!(
            w_draws.is_empty(),
            "Manual-redirected W must NOT emit (cow={cow_host_xid:?}): {:?}",
            scene.draws,
        );
    }
}

/// Issue #98 — an opaque, output-covering UNREDIRECTED top-level must
/// suppress the COW even when the compositor keeps a helper window
/// stacked ABOVE it, provided that helper lies entirely off-output.
///
/// Measured on eiger (Asahi, Cinnamon session): muffin parks 1x1
/// helper windows at (-200,-200) and raises them above the managed
/// stack. The top-down probe stopped on one of those, concluded "the
/// topmost window does not cover the output", and left the COW
/// painting the desktop composite over the window muffin had just
/// unredirected — so fullscreen video/games rendered as the wallpaper
/// while their audio kept playing.
#[test]
fn offscreen_helper_above_fullscreen_still_suppresses_cow() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    // The unredirected fullscreen window: covers the 800x600 output,
    // opaque (depth != 32), scene-participating (drawn by us).
    let fs: u32 = 0x00F5;
    alloc_stub_window(&mut store, &mut windows, fs, 0, 0, 800, 600, None, true);
    windows.get_mut(&fs).expect("fs geom").depth = 24;
    core.top_level_order.push(fs);

    // muffin's off-screen 1x1 helper, stacked above `fs`.
    let helper: u32 = 0x00AE;
    alloc_stub_window(
        &mut store,
        &mut windows,
        helper,
        -200,
        -200,
        1,
        1,
        None,
        true,
    );
    windows.get_mut(&helper).expect("helper geom").depth = 24;
    core.top_level_order.push(helper);

    // The COW, always on top.
    let cow: u32 = 0x0103;
    alloc_stub_window(&mut store, &mut windows, cow, 0, 0, 800, 600, None, true);
    core.top_level_order.push(cow);

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        Some(cow),
        false,
        Visibility::Off,
    );

    let cow_view: vk::ImageView = ash::vk::Handle::from_raw(u64::from(cow) | 0xFF00_0000);
    assert!(
        !built.scene.draws.iter().any(|d| d.image_view == cow_view),
        "COW must be suppressed by the opaque fullscreen unredirected \
             window even with an off-output helper stacked above it: {:?}",
        built.scene.draws,
    );
    let fs_view: vk::ImageView = ash::vk::Handle::from_raw(u64::from(fs) | 0xFF00_0000);
    assert!(
        built.scene.draws.iter().any(|d| d.image_view == fs_view),
        "the fullscreen window itself must still emit: {:?}",
        built.scene.draws,
    );
}

/// Issue #98 negative — the off-output filter must not turn into
/// blanket over-suppression. A window that IS on this output and does
/// NOT cover it (an ordinary floating window above the fullscreen one)
/// still keeps the COW alive; suppressing it there would erase every
/// redirected window the compositor draws, i.e. the whole desktop.
#[test]
fn on_output_non_covering_window_above_fullscreen_keeps_cow() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    let fs: u32 = 0x00F5;
    alloc_stub_window(&mut store, &mut windows, fs, 0, 0, 800, 600, None, true);
    windows.get_mut(&fs).expect("fs geom").depth = 24;
    core.top_level_order.push(fs);

    // On-output, non-covering window stacked above the fullscreen one.
    let float: u32 = 0x00BF;
    alloc_stub_window(
        &mut store,
        &mut windows,
        float,
        100,
        100,
        200,
        150,
        None,
        true,
    );
    windows.get_mut(&float).expect("float geom").depth = 24;
    core.top_level_order.push(float);

    let cow: u32 = 0x0103;
    alloc_stub_window(&mut store, &mut windows, cow, 0, 0, 800, 600, None, true);
    core.top_level_order.push(cow);

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        Some(cow),
        false,
        Visibility::Off,
    );

    let cow_view: vk::ImageView = ash::vk::Handle::from_raw(u64::from(cow) | 0xFF00_0000);
    assert!(
        built.scene.draws.iter().any(|d| d.image_view == cow_view),
        "COW must survive when the topmost on-output window does not \
             cover the output: {:?}",
        built.scene.draws,
    );
}

/// Phase 3.1 negative — an Automatic-redirected top-level (own
/// `redirected_target` + `scene_participating=true`) still emits
/// a draw. Only the Manual mode (the bug-shaped case the gate
/// closes) is unconditionally skipped.
#[test]
fn automatic_redirected_top_level_still_emits() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    let w: u32 = 0xA2;
    alloc_stub_window(&mut store, &mut windows, w, 100, 100, 50, 50, None, true);
    let w_id = store.lookup(w).expect("w lookup");
    let mut backing = crate::kms::render::store::Storage::for_tests_null(
        extent(50, 50),
        PlatformBackend::format_for_depth(24),
    );
    let view: vk::ImageView = ash::vk::Handle::from_raw(0xBEEF_0001);
    backing.image_view = view;
    backing.sample_view = view;
    let b_id = store
        .allocate(0xB0A2, DrawableKind::Pixmap, 24, true, backing)
        .expect("alloc automatic backing");
    store.set_redirected_target(w_id, Some(b_id));
    // scene_participating left as default true (Automatic).
    core.top_level_order.push(w);

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        None,
        false,
        Visibility::Off,
    );
    let scene = &built.scene;

    let w_draws: Vec<_> = scene
        .draws
        .iter()
        .filter(|d| d.dst_size == [50.0, 50.0])
        .collect();
    assert_eq!(
        w_draws.len(),
        1,
        "Automatic-redirected W still emits one draw: {:?}",
        scene.draws,
    );
}

/// Phase 6.1 — full compositor flow in one scenario. Exercises the
/// structural facts the COW redesign delivers, headless via a
/// direct `build_scene` call (no live Vulkan device required):
///
/// 1. A materialized COW (`windows` entry + `top_level_order`
///    slot, per Task 2.2) emits exactly once via the normal
///    `top_level_order` walk (Phase 2.7), with
///    `alpha_passthrough=true` (Phase 2.6).
/// 2. A stage child of the COW with content emits exactly once via
///    the COW-subtree recursion (Phase 2.6/2.7), also
///    `alpha_passthrough=true`.
/// 3. A Manual-redirected sibling top-level (own
///    `redirected_target` + `scene_participating=false`) emits
///    ZERO draws (Phase 3.1) — even though the COW is materialized.
/// 4. Ordering: the COW-subtree draws appear after the earlier
///    non-COW top-level (the Manual sibling contributes nothing).
///
/// Sizes are chosen so each source is unambiguously identifiable by
/// `dst_size`:
///   - early non-COW top-level W: 200×200
///   - Manual-redirected sibling S: 50×50  (must not appear)
///   - COW host:                    800×600
///   - stage (COW child):           640×480
#[test]
fn compositor_stage_under_cow_emits_via_recursion_and_manual_siblings_skip() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let platform = PlatformBackend::for_tests();
    let mut windows = crate::kms::render::backend::WindowsMap::new();

    // (1) An earlier, ordinary non-COW top-level W @ (0,0), 200×200.
    // Establishes a "before" position to anchor ordering.
    let w: u32 = 0xC001;
    alloc_stub_window(&mut store, &mut windows, w, 0, 0, 200, 200, None, true);
    core.top_level_order.push(w);

    // (2) A Manual-redirected sibling top-level S @ (100,100), 50×50.
    // redirected_target + scene_participating=false → Manual mode.
    let s: u32 = 0xC002;
    alloc_stub_window(&mut store, &mut windows, s, 100, 100, 50, 50, None, true);
    let s_id = store.lookup(s).expect("s lookup");
    let mut s_backing = crate::kms::render::store::Storage::for_tests_null(
        extent(50, 50),
        PlatformBackend::format_for_depth(24),
    );
    let s_view: vk::ImageView = ash::vk::Handle::from_raw(0xDEAD_0050);
    s_backing.image_view = s_view;
    s_backing.sample_view = s_view;
    let s_backing_id = store
        .allocate(0xB0C2, DrawableKind::Pixmap, 24, true, s_backing)
        .expect("alloc manual sibling backing");
    store.set_redirected_target(s_id, Some(s_backing_id));
    store.set_scene_participating(s_id, false);
    core.top_level_order.push(s);

    // (3) The materialized COW host @ (0,0), 800×600 (matches the
    // PlatformBackend::for_tests output extent). This stands in for
    // GetOverlayWindow having created the windows entry +
    // top_level_order slot (Task 2.2).
    let cow_xid: u32 = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
    alloc_stub_window(
        &mut store,
        &mut windows,
        cow_xid,
        0,
        0,
        800,
        600,
        None,
        true,
    );
    core.top_level_order.push(cow_xid);

    // (4) The compositor stage as a child of the COW @ (0,0),
    // 640×480 — content the WM paints into the overlay.
    let stage: u32 = 0xC003;
    alloc_stub_window(
        &mut store,
        &mut windows,
        stage,
        0,
        0,
        640,
        480,
        Some(cow_xid),
        true,
    );

    let built = build_scene(
        &core,
        &mut store,
        &windows,
        0,
        &platform,
        None,
        None,
        Some(cow_xid),
        false,
        Visibility::Off,
    );
    let scene = &built.scene;

    // Fact A — Manual-redirected sibling S emits ZERO draws.
    let s_draws = scene
        .draws
        .iter()
        .filter(|d| d.dst_size == [50.0, 50.0])
        .count();
    assert_eq!(
        s_draws, 0,
        "Manual-redirected sibling must not emit, even with COW materialized: {:?}",
        scene.draws,
    );

    // Fact B — stage (COW child) emits exactly ONE draw with
    // alpha_passthrough=true.
    let stage_draws: Vec<_> = scene
        .draws
        .iter()
        .filter(|d| d.dst_size == [640.0, 480.0])
        .collect();
    assert_eq!(
        stage_draws.len(),
        1,
        "stage emits exactly once via COW subtree recursion: {:?}",
        scene.draws,
    );
    assert!(
        stage_draws[0].alpha_passthrough,
        "stage draw inherits alpha_passthrough=true from the COW subtree: {:?}",
        stage_draws[0],
    );

    // Fact C — COW emits exactly ONE draw with alpha_passthrough=true
    // via the normal top_level_order walk (no special post-walk append).
    let cow_draws: Vec<_> = scene
        .draws
        .iter()
        .filter(|d| d.dst_size == [800.0, 600.0])
        .collect();
    assert_eq!(
        cow_draws.len(),
        1,
        "COW emits exactly once via top_level_order walk: {:?}",
        scene.draws,
    );
    assert!(
        cow_draws[0].alpha_passthrough,
        "COW draw has alpha_passthrough=true: {:?}",
        cow_draws[0],
    );

    // The earlier non-COW top-level W emits one opaque draw.
    let w_pos = scene
        .draws
        .iter()
        .position(|d| d.dst_size == [200.0, 200.0])
        .expect("W draw present");
    assert!(
        !scene.draws[w_pos].alpha_passthrough,
        "non-COW top-level W uses opaque blend (alpha_passthrough=false)",
    );

    // Fact D — ordering: the COW-subtree draws (COW host + stage)
    // appear AFTER the earlier non-COW top-level W. The Manual
    // sibling contributes nothing in between.
    let cow_pos = scene
        .draws
        .iter()
        .position(|d| d.dst_size == [800.0, 600.0])
        .expect("COW draw present");
    let stage_pos = scene
        .draws
        .iter()
        .position(|d| d.dst_size == [640.0, 480.0])
        .expect("stage draw present");
    assert!(
        w_pos < cow_pos && w_pos < stage_pos,
        "COW subtree draws come after the earlier top-level W: w={w_pos} cow={cow_pos} stage={stage_pos}",
    );
    // Within the COW subtree the host emits before its stage child.
    assert!(
        cow_pos < stage_pos,
        "COW host draw precedes its stage child in the subtree recursion: cow={cow_pos} stage={stage_pos}",
    );
}

// ── Step 1 stage A: the refactored emitter is a no-op ─────────────
//
// `legacy_emit_window_subtree` is the emitter as it stood before the
// per-node decision was factored out (`decide_node`) and the children
// index replaced the per-node `WindowsMap` scan. It is kept verbatim so the
// refactor can be checked against it on trees the WM-shaped tests do not
// build: deep nesting, overlapping siblings, shaped nodes, children that
// extend beyond their parent, manual/automatic redirect chains, a COW
// subtree, a non-zero layout origin, a window straddling the output edge.
// Delete it together with this test once stage B changes what is emitted.
/// Verbatim pre-refactor emitter (2026-09-03), test twin only.
fn legacy_emit_window_subtree(
    host_xid: u32,
    parent_abs_x: i32,
    parent_abs_y: i32,
    store: &mut DrawableStore,
    windows: &crate::kms::render::backend::WindowsMap,
    // Per-window SHAPE bounding regions (`KmsCore::shape_bounding`).
    // When a host xid has an entry the window's scene draw is
    // clipped to those rects — marco's rounded-corner frame masks
    // depend on this. Empty / missing entry → unshaped, single
    // full-window draw.
    shape_bounding: &HashMap<u32, Vec<xfixes::RegionRect>>,
    layout_x0: i32,
    layout_y0: i32,
    layout_w: u32,
    layout_h: u32,
    draws: &mut Vec<CompositeDraw>,
    snapshots: &mut Vec<DamageSnapshot>,
    sampled_ids: &mut Vec<crate::kms::render::store::DrawableId>,
    projected: &mut RegionSet,
    // Step 2 — one presence per participant that emits, region derived from the
    // draws it pushed. Threaded rather than returned so the recursion can append
    // in emission order.
    participants: &mut Vec<ScenePresence>,
    // Audit #3 (2026-05-19): true iff some ancestor on the recursion
    // path owns a `redirected_target`. When set, this window's paint
    // landed in that ancestor's backing (via `resolve_paint_target`'s
    // ancestor walk), so emitting this window's own storage would
    // show stale/empty pixels — the ancestor's emit already shows
    // the content. A descendant that owns ITS OWN `redirected_target`
    // breaks this chain (its paint stops at itself), so it still
    // emits its own backing regardless of the inherited flag.
    under_redirected_ancestor: bool,
    // Phase 2.6 — true iff the current recursion path entered the
    // COW top-level (or one of its descendants). When set, emitted
    // `CompositeDraw` entries take `alpha_passthrough = true` so the
    // compositor's composited result blends over the layer below;
    // outside the COW subtree (no compositor active) draws stay
    // opaque (`alpha_passthrough = false`). Mirrors the threading of
    // `under_redirected_ancestor` above.
    under_cow_subtree: bool,
    // X11 parent-clipping: a window's visible region is the
    // intersection of its own rectangle with EVERY ancestor's
    // rectangle. These are the accumulated ancestor bounds in absolute
    // screen coords (half-open [x0,x1) × [y0,y1)); this window's draw
    // and its descendants' clips are intersected against them. The
    // top-level call passes effectively-unbounded bounds (top-levels
    // are screen-clipped by the output-extent gate), so this is a
    // no-op for the common case where children fit inside their
    // parents — it only bites a child that extends beyond its parent,
    // e.g. an fvwm frame decoration parked in a tiny holding window.
    clip_x0: i32,
    clip_y0: i32,
    clip_x1: i32,
    clip_y1: i32,
) {
    let debug_focus = scene_walk_debug_enabled_for(host_xid);
    // Stage 4 diagnostic: trace-level scene-walk decision per window.
    // Enable with `RUST_LOG=yserver::kms::render::scene=trace`. The
    // top-level and descendant paths share this function so the
    // single trace site covers both. Format is greppable —
    // `render scene_walk xid=...: ...` — for `grep "render scene_walk"`
    // over yserver-hw.log to extract just these lines.
    let Some(geom) = windows.get(&host_xid) else {
        log::trace!("render scene_walk xid={host_xid:#x}: SKIP reason=geom_not_in_windows");
        if debug_focus {
            log::debug!("render scene_walk xid={host_xid:#x}: SKIP reason=geom_not_in_windows");
        }
        return;
    };
    if !geom.mapped {
        // X11: an unmapped window (and entire subtree) is invisible.
        log::trace!(
            "render scene_walk xid={host_xid:#x}: SKIP reason=geom_unmapped \
                 geom=({x},{y} {w}x{h}) depth={depth} parent={parent:?}",
            x = geom.x,
            y = geom.y,
            w = geom.width,
            h = geom.height,
            depth = geom.depth,
            parent = geom.parent,
        );
        if debug_focus {
            log::debug!(
                "render scene_walk xid={host_xid:#x}: SKIP reason=geom_unmapped \
                     geom=({x},{y} {w}x{h}) depth={depth} parent={parent:?}",
                x = geom.x,
                y = geom.y,
                w = geom.width,
                h = geom.height,
                depth = geom.depth,
                parent = geom.parent,
            );
        }
        return;
    }
    let abs_x = parent_abs_x + i32::from(geom.x);
    let abs_y = parent_abs_y + i32::from(geom.y);

    // X11 parent-clipping. This window's visible box in its OWN local
    // coords = its rect [0,own_w)×[0,own_h) intersected with the
    // accumulated ancestor clip (translated into local coords). Draws
    // are restricted to this box; descendants inherit the intersection
    // (in absolute coords) as their clip. `vis_*` empty ⇒ nothing of
    // this window is visible (fully clipped by an ancestor).
    let own_w = i32::from(geom.width);
    let own_h = i32::from(geom.height);
    let vis_lx0 = (clip_x0 - abs_x).max(0);
    let vis_ly0 = (clip_y0 - abs_y).max(0);
    let vis_lx1 = (clip_x1 - abs_x).min(own_w);
    let vis_ly1 = (clip_y1 - abs_y).min(own_h);
    // Absolute clip passed down to children = ancestor clip ∩ own rect.
    let child_clip_x0 = clip_x0.max(abs_x);
    let child_clip_y0 = clip_y0.max(abs_y);
    let child_clip_x1 = clip_x1.min(abs_x + own_w);
    let child_clip_y1 = clip_y1.min(abs_y + own_h);

    // Manual-redirect subtree boundary. When a window is
    // `scene_participating=false` here, the compositor owns the
    // entire subtree's presentation (X11 Composite §285+360 —
    // Manual-mode redirect removes the window AND its descendants
    // from normal scene-out; the compositor reads the redirected
    // backing instead). Set after the per-node decision so we
    // can return *after* the SKIP trace fires (preserves the
    // existing trace shape for live debugging) and before the
    // child-recurse below.
    //
    // Audit #3 (2026-05-19): the old `prune_subtree=true` for
    // `scene_participating=false` is gone — Automatic descendants of
    // Manual ancestors need to recurse so they can emit their own
    // backing. Per-window emit-vs-skip is decided by
    // `paint_target_is_self` below; the recurse always runs and the
    // `under_redirected_ancestor` flag carries the chain context.

    // Emit a draw entry for this window if it has live storage that
    // participates in the scene.
    let lookup_id = store.lookup(host_xid);
    if lookup_id.is_none() {
        log::trace!(
            "render scene_walk xid={host_xid:#x}: SKIP reason=no_store_lookup \
                 geom=({x},{y} {w}x{h}) mapped=true depth={depth}",
            x = geom.x,
            y = geom.y,
            w = geom.width,
            h = geom.height,
            depth = geom.depth,
        );
        if debug_focus {
            log::debug!(
                "render scene_walk xid={host_xid:#x}: SKIP reason=no_store_lookup \
                     geom=({x},{y} {w}x{h}) mapped=true depth={depth}",
                x = geom.x,
                y = geom.y,
                w = geom.width,
                h = geom.height,
                depth = geom.depth,
            );
        }
    }
    if let Some(id) = lookup_id {
        // Pull diagnostic fields up front (cheap copies) so we can
        // emit a single SKIP/WILL_EMIT trace line per gate failure
        // without re-borrowing the store across log call sites.
        let drawable_snap = store.get(id).map(|d| {
            (
                d.id,
                d.kind,
                d.depth,
                d.refcount,
                d.scene_participating,
                d.storage.extent,
                d.storage.image_view == vk::ImageView::null(),
            )
        });
        if let Some((d_id, d_kind, d_depth, d_refcount, d_part, d_extent, d_view_null)) =
            drawable_snap
        {
            // Stage 4c.3 — route source-storage through `redirected_target`.
            // Both modes blit FROM B; W's geometry (dst_origin, dst_size,
            // intersect test) stays driven by W's own state in
            // `windows`. Only the sampled storage handle reroutes.
            let source_id = store.redirected_target(id).unwrap_or(id);
            let source_view_null = store
                .get(source_id)
                .is_none_or(|s| s.storage.image_view == vk::ImageView::null());

            // Audit #3 (2026-05-19) — emit-or-skip is governed by
            // "is this window's storage where paint actually lands?"
            //
            //   has_own_redirected_target   self owns a `redirected_target`
            //                               → paint lands in its B, emit B.
            //   under_redirected_ancestor   some ancestor owns one
            //                               → paint lands in ancestor's B,
            //                                 ancestor emits it, we skip.
            //   d_part                      `scene_participating=true` —
            //                                 ordinary non-redirected window
            //                                 with its own storage as the
            //                                 paint target. Emit own storage.
            //
            // Pre-fix the rule was `d_part || manual_backing_visible`
            // plus an unconditional `prune_subtree` on
            // `scene_participating=false`. That dropped Automatic-
            // redirected descendants of Manual-redirected ancestors —
            // GTK/marco CSD frames lose their inner widgets (per audit
            // #3 / Control Center missing-widget reports).
            let has_own_redirected_target = source_id != id;
            // Phase 3.1 — Manual-redirected windows (own a
            // `redirected_target` AND `scene_participating=false`)
            // must NEVER emit to scanout. They go offscreen for the
            // compositor to read via NameWindowPixmap; the X server
            // must not also blit the backing in. Mirrors Xorg's
            // structural guarantee from `compCheckRedirect`.
            let is_manual_redirected = has_own_redirected_target && !d_part;
            let paint_target_is_self = !is_manual_redirected
                && (has_own_redirected_target || (d_part && !under_redirected_ancestor));

            // Project onto output-local coords (computed once here so
            // both the SKIP=no_intersect and WILL_EMIT trace lines can
            // include the dst rect).
            let dx = abs_x - layout_x0;
            let dy = abs_y - layout_y0;
            let win_w = i32::from(geom.width);
            let win_h = i32::from(geom.height);
            let intersects = !(dx + win_w <= 0
                || dy + win_h <= 0
                || dx >= i32::try_from(layout_w).unwrap_or(i32::MAX)
                || dy >= i32::try_from(layout_h).unwrap_or(i32::MAX));

            // Pick the first failing gate and emit a single SKIP line;
            // otherwise emit WILL_EMIT. Order matches the production
            // gate ordering below so the trace mirrors the live path.
            let skip_reason: Option<&'static str> = if is_manual_redirected {
                // Phase 3.1 — first reason in the cascade. A
                // Manual-redirected window (own redirected_target +
                // scene_participating=false) is unconditionally
                // skipped; the compositor reads its backing via
                // NameWindowPixmap and re-emits it on the COW.
                Some("manual_redirect_unconditional_skip")
            } else if !paint_target_is_self {
                if has_own_redirected_target {
                    // Defensive — `paint_target_is_self` is true when
                    // `has_own_redirected_target` AND not
                    // Manual-redirected (the Manual case is handled
                    // by the branch above), so this branch is
                    // unreachable. Kept so the match stays exhaustive
                    // if the rule ever evolves.
                    Some("paint_target_not_self")
                } else if under_redirected_ancestor {
                    Some("paint_target_is_redirected_ancestor")
                } else {
                    Some("scene_participating=false")
                }
            } else if !matches!(d_kind, DrawableKind::Window) {
                Some("kind!=Window")
            } else if source_view_null {
                Some("source_image_view_null")
            } else if !intersects {
                Some("no_intersect_with_output")
            } else {
                None
            };

            if debug_focus {
                log::debug!(
                    "render scene_walk focus xid={host_xid:#x} source_id={source_id:?} \
                         has_own_redirected_target={has_own_redirected_target} \
                         under_redirected_ancestor={under_redirected_ancestor} \
                         paint_target_is_self={paint_target_is_self} \
                         intersects={intersects} skip_reason={skip_reason:?}",
                );
            }

            if let Some(reason) = skip_reason {
                log::trace!(
                    "render scene_walk xid={host_xid:#x}: SKIP reason={reason} \
                         geom=({gx},{gy} {gw}x{gh}) mapped=true \
                         store_id={d_id:?} kind={d_kind:?} depth={d_depth} \
                         refcount={d_refcount} scene_participating={d_part} \
                         storage_extent={dew}x{deh} image_view_null={d_view_null} \
                         source_id={source_id:?} source_view_null={source_view_null}",
                    gx = geom.x,
                    gy = geom.y,
                    gw = geom.width,
                    gh = geom.height,
                    dew = d_extent.width,
                    deh = d_extent.height,
                );
                if debug_focus {
                    log::debug!(
                        "render scene_walk xid={host_xid:#x}: SKIP reason={reason} \
                             geom=({gx},{gy} {gw}x{gh}) mapped=true \
                             store_id={d_id:?} kind={d_kind:?} depth={d_depth} \
                             refcount={d_refcount} scene_participating={d_part} \
                             storage_extent={dew}x{deh} image_view_null={d_view_null} \
                             source_id={source_id:?} source_view_null={source_view_null}",
                        gx = geom.x,
                        gy = geom.y,
                        gw = geom.width,
                        gh = geom.height,
                        dew = d_extent.width,
                        deh = d_extent.height,
                    );
                }
            } else {
                log::trace!(
                    "render scene_walk xid={host_xid:#x}: WILL_EMIT \
                         geom=({gx},{gy} {gw}x{gh}) abs=({abs_x},{abs_y}) \
                         output=({dx},{dy} {win_w}x{win_h}) \
                         store_id={d_id:?} kind={d_kind:?} depth={d_depth} \
                         refcount={d_refcount} scene_participating={d_part} \
                         storage_extent={dew}x{deh} image_view_null={d_view_null} \
                         source_id={source_id:?}",
                    gx = geom.x,
                    gy = geom.y,
                    gw = geom.width,
                    gh = geom.height,
                    dew = d_extent.width,
                    deh = d_extent.height,
                );
                if debug_focus {
                    log::debug!(
                        "render scene_walk xid={host_xid:#x}: WILL_EMIT \
                             geom=({gx},{gy} {gw}x{gh}) abs=({abs_x},{abs_y}) \
                             output=({dx},{dy} {win_w}x{win_h}) \
                             store_id={d_id:?} kind={d_kind:?} depth={d_depth} \
                             refcount={d_refcount} scene_participating={d_part} \
                             storage_extent={dew}x{deh} image_view_null={d_view_null} \
                             source_id={source_id:?}",
                        gx = geom.x,
                        gy = geom.y,
                        gw = geom.width,
                        gh = geom.height,
                        dew = d_extent.width,
                        deh = d_extent.height,
                    );
                }
            }

            if matches!(d_kind, DrawableKind::Window)
                && let Some(source) = store.get(source_id)
                && source.storage.image_view != vk::ImageView::null()
                && intersects
                && paint_target_is_self
            {
                // Window scene draw — bind the sample-side view
                // (format/depth-aware swizzle) instead of the
                // raw IDENTITY-swizzle attachment view. This is
                // the load-bearing fix for the "depth-24 windows
                // / COW α leak" bug: the BgraNoAlpha swizzle
                // forced α=ONE for depth-24 used to live ONLY in
                // the engine's RENDER view-cache, never on the
                // scene path. Combined with `alpha_passthrough=true`
                // below, the prior IDENTITY view leaked the
                // BGRA8 padding byte (typically 0) into the
                // shader's `src.a`, blending depth-24 windows
                // with α=0 — invisible against root, which
                // matched the post-4d.7 mate-with-compositing
                // and xfce-with-compositing hardware-smoke
                // failure shape.
                //
                // SHAPE bounding handling: when the window has a
                // bounding region (marco's rounded-corner mask,
                // panel-applet transparency cutouts, etc.) emit
                // one clipped draw per rect intersected with the
                // window's storage extent. Without bounding (the
                // common case), emit a single full-window draw —
                // preserving the alpha-passthrough invariants
                // documented above for the depth-32 / depth-24
                // distinction. Pixels outside the bounding region
                // are intentionally NOT drawn so the layer below
                // (parent / wallpaper / root) shows through.
                let image_view = source.storage.sample_view;
                #[allow(clippy::cast_precision_loss)]
                let win_w_f = win_w as f32;
                #[allow(clippy::cast_precision_loss)]
                let win_h_f = win_h as f32;
                let mut emitted_any = false;
                let draw_start = draws.len();
                if let Some(rects) = shape_bounding.get(&host_xid) {
                    for rect in rects {
                        let rx = i32::from(rect.x);
                        let ry = i32::from(rect.y);
                        let rw = i32::from(rect.width);
                        let rh = i32::from(rect.height);
                        // Clamp to the window extent AND the ancestor
                        // visible box (parent-clipping).
                        let cx = rx.max(0).max(vis_lx0);
                        let cy = ry.max(0).max(vis_ly0);
                        let cw = (rx + rw).min(win_w).min(vis_lx1) - cx;
                        let ch = (ry + rh).min(win_h).min(vis_ly1) - cy;
                        if cw <= 0 || ch <= 0 {
                            continue;
                        }
                        #[allow(clippy::cast_precision_loss)]
                        let cw_f = cw as f32;
                        #[allow(clippy::cast_precision_loss)]
                        let ch_f = ch as f32;
                        #[allow(clippy::cast_precision_loss)]
                        let cx_f = cx as f32;
                        #[allow(clippy::cast_precision_loss)]
                        let cy_f = cy as f32;
                        draws.push(CompositeDraw {
                            image_view,
                            #[allow(clippy::cast_precision_loss)]
                            dst_origin: [(dx + cx) as f32, (dy + cy) as f32],
                            dst_size: [cw_f, ch_f],
                            src_origin: [cx_f / win_w_f, cy_f / win_h_f],
                            src_size: [cw_f / win_w_f, ch_f / win_h_f],
                            // Phase 2.6 — alpha-passthrough is inherited
                            // from the COW subtree flag (set on the COW
                            // top-level + descendants). Outside the COW
                            // subtree, draws stay opaque.
                            alpha_passthrough: under_cow_subtree,
                        });
                        emitted_any = true;
                    }
                } else if vis_lx1 > vis_lx0 && vis_ly1 > vis_ly0 {
                    // Unshaped: emit the window rect clipped to the
                    // ancestor visible box. Common case (child fits
                    // inside its parent) → box == full window, so this
                    // is the full-window draw with src [0,0]-[1,1].
                    let cw = vis_lx1 - vis_lx0;
                    let ch = vis_ly1 - vis_ly0;
                    #[allow(clippy::cast_precision_loss)]
                    draws.push(CompositeDraw {
                        image_view,
                        dst_origin: [(dx + vis_lx0) as f32, (dy + vis_ly0) as f32],
                        dst_size: [cw as f32, ch as f32],
                        src_origin: [vis_lx0 as f32 / win_w_f, vis_ly0 as f32 / win_h_f],
                        src_size: [cw as f32 / win_w_f, ch as f32 / win_h_f],
                        // Phase 2.6 — alpha-passthrough is inherited
                        // from the COW subtree flag (set on the COW
                        // top-level + descendants). Outside the COW
                        // subtree, draws stay opaque (no compositor
                        // path); inside the COW subtree, the
                        // compositor's stage paints with alpha and we
                        // blend over whatever lies below.
                        alpha_passthrough: under_cow_subtree,
                    });
                    emitted_any = true;
                }
                if emitted_any {
                    sampled_ids.push(source_id);
                    // Region unioned across every draw this window pushed, so a
                    // shaped window emitting one quad per shape rect is ONE
                    // participant. Identity is the host drawable, so a redirect
                    // swap is a resample rather than a replacement.
                    if let Some(p) = legacy_presence_from_draws(
                        draws,
                        draw_start,
                        ParticipantId {
                            role: SceneRole::Window,
                            xid: host_xid,
                            generation: d_id.as_u64(),
                        },
                    ) {
                        participants.push(p);
                    }
                    if let Some(snap) = store.peek_presentation_damage(source_id) {
                        for r in snap.region.rects() {
                            add_projected_damage(projected, *r, dx, dy, layout_w, layout_h);
                        }
                        snapshots.push(snap);
                    }
                }
            }
        } else {
            log::trace!(
                "render scene_walk xid={host_xid:#x}: SKIP reason=store_get_returned_none \
                     store_id={lookup_id:?} geom=({x},{y} {w}x{h}) mapped=true depth={depth}",
                x = geom.x,
                y = geom.y,
                w = geom.width,
                h = geom.height,
                depth = geom.depth,
            );
            if debug_focus {
                log::debug!(
                    "render scene_walk xid={host_xid:#x}: SKIP reason=store_get_returned_none \
                         store_id={lookup_id:?} geom=({x},{y} {w}x{h}) mapped=true depth={depth}",
                    x = geom.x,
                    y = geom.y,
                    w = geom.width,
                    h = geom.height,
                    depth = geom.depth,
                );
            }
        }
    }

    // Audit #3 (2026-05-19) — descendants need to know whether THEY
    // sit under a redirected ancestor. The chain is "this window
    // counts as a redirected ancestor iff it owns its own
    // `redirected_target`" — that's exactly where
    // `resolve_paint_target` stops climbing the parent chain. A
    // recursion under a Manual-redirected ancestor without own
    // backing flips the flag on; an Automatic-redirected descendant
    // beneath that resets the flag for its own descendants (because
    // its paint stops at its own B).
    let self_owns_redirected_target = store
        .lookup(host_xid)
        .and_then(|id| store.redirected_target(id))
        .is_some();
    let child_under_redirected_ancestor = under_redirected_ancestor || self_owns_redirected_target;

    // Recurse into mapped descendants in stable sibling stack order.
    let mut children: Vec<(u32, u64)> = windows
        .iter()
        .filter_map(|(xid, g)| {
            if g.parent == Some(host_xid) {
                Some((*xid, g.stack_rank))
            } else {
                None
            }
        })
        .collect();
    children.sort_by_key(|(_, rank)| *rank);
    for (child_xid, _) in children {
        legacy_emit_window_subtree(
            child_xid,
            abs_x,
            abs_y,
            store,
            windows,
            shape_bounding,
            layout_x0,
            layout_y0,
            layout_w,
            layout_h,
            draws,
            snapshots,
            sampled_ids,
            projected,
            participants,
            child_under_redirected_ancestor,
            // Phase 2.6 — COW subtree flag is inherited unchanged.
            // Once we entered the COW top-level, every descendant
            // emits with alpha_passthrough=true.
            under_cow_subtree,
            // Parent-clipping: children are clipped to this window's
            // rect intersected with the inherited ancestor clip.
            child_clip_x0,
            child_clip_y0,
            child_clip_x1,
            child_clip_y1,
        );
    }
}

#[derive(Debug, PartialEq, Eq)]
struct DrawKey {
    view: u64,
    dst_origin: [u32; 2],
    dst_size: [u32; 2],
    src_origin: [u32; 2],
    src_size: [u32; 2],
    alpha_passthrough: bool,
}

fn draw_key(d: &CompositeDraw) -> DrawKey {
    DrawKey {
        view: ash::vk::Handle::as_raw(d.image_view),
        dst_origin: d.dst_origin.map(f32::to_bits),
        dst_size: d.dst_size.map(f32::to_bits),
        src_origin: d.src_origin.map(f32::to_bits),
        src_size: d.src_size.map(f32::to_bits),
        alpha_passthrough: d.alpha_passthrough,
    }
}

#[derive(Debug, PartialEq, Eq)]
struct WalkOut {
    draws: Vec<DrawKey>,
    participants: Vec<ScenePresence>,
    sampled: Vec<crate::kms::render::store::DrawableId>,
    snapshots: Vec<(crate::kms::render::store::DrawableId, u64)>,
    projected: Vec<vk::Rect2D>,
}

/// The pre-step-1 presence constructor, kept verbatim for the legacy
/// emitter: region = union of the emitted dst rects (outward-rounded),
/// signature from the first draw. `visible` did not exist; it equals the
/// region, which is what `Visibility::Off` produces too.
fn legacy_presence_from_draws(
    draws: &[CompositeDraw],
    from: usize,
    id: ParticipantId,
) -> Option<ScenePresence> {
    let emitted = draws.get(from..)?;
    let first = emitted.first()?;
    let mut region = Region::new();
    let mut place = Vec::new();
    for d in emitted {
        let x0 = d.dst_origin[0].floor();
        let y0 = d.dst_origin[1].floor();
        let x1 = (d.dst_origin[0] + d.dst_size[0]).ceil();
        let y1 = (d.dst_origin[1] + d.dst_size[1]).ceil();
        if x1 > x0 && y1 > y0 {
            #[allow(clippy::cast_possible_truncation)]
            let r = vk::Rect2D {
                offset: vk::Offset2D {
                    x: x0 as i32,
                    y: y0 as i32,
                },
                extent: vk::Extent2D {
                    width: (x1 - x0) as u32,
                    height: (y1 - y0) as u32,
                },
            };
            region.add_rect(r);
            place.push(r);
        }
    }
    if region.is_empty() {
        return None;
    }
    Some(ScenePresence {
        id,
        visible: region.clone(),
        region,
        place,
        signature: PresenceSignature::new(
            first.image_view,
            first.src_origin,
            first.src_size,
            first.alpha_passthrough,
        ),
    })
}

fn platform_with_layout(layout: (i32, i32, u32, u32)) -> PlatformBackend {
    let (lx, ly, lw, lh) = layout;
    let mut platform = PlatformBackend::for_tests();
    let out = &mut platform.outputs[0];
    out.x = lx;
    out.y = ly;
    out.width = u16::try_from(lw).expect("test layout width");
    out.height = u16::try_from(lh).expect("test layout height");
    platform
}

/// `build_scene` on a single output at `layout`, no cursor.
fn build_with(
    mode: Visibility,
    core: &KmsCore,
    store: &mut DrawableStore,
    windows: &crate::kms::render::backend::WindowsMap,
    layout: (i32, i32, u32, u32),
    cow_host_xid: Option<u32>,
) -> SceneBuild {
    let platform = platform_with_layout(layout);
    build_scene(
        core,
        store,
        windows,
        0,
        &platform,
        None,
        None,
        cow_host_xid,
        false,
        mode,
    )
}

/// `build_with` for one output of a multi-output layout: `elsewhere` is
/// what the OTHER output(s) showed at their last walk (their `pieces_ids`).
fn build_with_elsewhere(
    mode: Visibility,
    core: &KmsCore,
    store: &mut DrawableStore,
    windows: &crate::kms::render::backend::WindowsMap,
    layout: (i32, i32, u32, u32),
    cow_host_xid: Option<u32>,
    elsewhere: &std::collections::HashSet<crate::kms::render::store::DrawableId>,
) -> SceneBuild {
    let platform = platform_with_layout(layout);
    build_scene_with(
        core,
        store,
        windows,
        0,
        &platform,
        None,
        None,
        cow_host_xid,
        false,
        mode,
        elsewhere,
    )
}

fn sorted_rects(mut rects: Vec<vk::Rect2D>) -> Vec<vk::Rect2D> {
    rects.sort_by_key(|r| (r.offset.y, r.offset.x, r.extent.height, r.extent.width));
    rects
}

fn walk_out_of(built: &SceneBuild) -> WalkOut {
    WalkOut {
        draws: built.scene.draws.iter().map(draw_key).collect(),
        participants: built.participants.clone(),
        sampled: built.sampled_ids.clone(),
        snapshots: built.snapshots.iter().map(|s| (s.id, s.epoch)).collect(),
        projected: sorted_rects(built.projected_damage.rects().to_vec()),
    }
}

/// Run the top-level walk with the LEGACY emitter (`legacy == true`) or the
/// real `build_scene` under `Visibility::Off`, and normalise the output.
/// The fixture has no root drawable, so the two lists line up one to one.
fn walk_with(
    legacy: bool,
    core: &KmsCore,
    store: &mut DrawableStore,
    windows: &crate::kms::render::backend::WindowsMap,
    layout: (i32, i32, u32, u32),
    cow_host_xid: Option<u32>,
) -> WalkOut {
    if !legacy {
        let built = build_with(Visibility::Off, core, store, windows, layout, cow_host_xid);
        return walk_out_of(&built);
    }
    let (lx, ly, lw, lh) = layout;
    let mut draws = Vec::new();
    let mut snapshots = Vec::new();
    let mut sampled = Vec::new();
    let mut projected = RegionSet::new();
    let mut participants = Vec::new();
    for &top in &core.top_level_order {
        let under_cow = Some(top) == cow_host_xid;
        legacy_emit_window_subtree(
            top,
            0,
            0,
            store,
            windows,
            &core.shape_bounding,
            lx,
            ly,
            lw,
            lh,
            &mut draws,
            &mut snapshots,
            &mut sampled,
            &mut projected,
            &mut participants,
            false,
            under_cow,
            i32::MIN / 2,
            i32::MIN / 2,
            i32::MAX / 2,
            i32::MAX / 2,
        );
    }
    WalkOut {
        draws: draws.iter().map(draw_key).collect(),
        participants,
        sampled,
        snapshots: snapshots.iter().map(|s| (s.id, s.epoch)).collect(),
        projected: sorted_rects(projected.rects().to_vec()),
    }
}

fn set_rank(windows: &mut crate::kms::render::backend::WindowsMap, xid: u32, rank: u64) {
    windows.get_mut(&xid).expect("window present").stack_rank = rank;
}

fn alloc_backing(
    store: &mut DrawableStore,
    xid: u32,
    w: u32,
    h: u32,
) -> crate::kms::render::store::DrawableId {
    let mut storage = crate::kms::render::store::Storage::for_tests_null(
        extent(w, h),
        vk::Format::B8G8R8A8_UNORM,
    );
    let view: vk::ImageView = ash::vk::Handle::from_raw(u64::from(xid) | 0xB000_0000);
    storage.image_view = view;
    storage.sample_view = view;
    store
        .allocate(xid, DrawableKind::Pixmap, 32, true, storage)
        .expect("alloc backing stub")
}

/// The tree every differential case runs on. Ranks are all distinct so
/// sibling order does not depend on `HashMap` iteration.
fn differential_fixture() -> (
    KmsCore,
    DrawableStore,
    crate::kms::render::backend::WindowsMap,
) {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    let mut rank = 1u64;
    let mut add = |store: &mut DrawableStore,
                   windows: &mut crate::kms::render::backend::WindowsMap,
                   xid: u32,
                   x: i16,
                   y: i16,
                   w: u16,
                   h: u16,
                   parent: Option<u32>,
                   mapped: bool| {
        alloc_stub_window(store, windows, xid, x, y, w, h, parent, mapped);
        set_rank(windows, xid, rank);
        rank += 1;
    };

    // Nesting three deep.
    add(
        &mut store,
        &mut windows,
        0x100,
        10,
        10,
        300,
        200,
        None,
        true,
    );
    add(
        &mut store,
        &mut windows,
        0x101,
        20,
        20,
        200,
        100,
        Some(0x100),
        true,
    );
    add(
        &mut store,
        &mut windows,
        0x102,
        30,
        30,
        50,
        40,
        Some(0x101),
        true,
    );
    // Unmapped child with a mapped grandchild: whole subtree hidden.
    add(
        &mut store,
        &mut windows,
        0x103,
        5,
        5,
        50,
        50,
        Some(0x100),
        false,
    );
    add(
        &mut store,
        &mut windows,
        0x104,
        1,
        1,
        10,
        10,
        Some(0x103),
        true,
    );
    // Overlapping siblings.
    add(
        &mut store,
        &mut windows,
        0x200,
        100,
        100,
        150,
        150,
        None,
        true,
    );
    add(
        &mut store,
        &mut windows,
        0x201,
        200,
        150,
        150,
        150,
        None,
        true,
    );
    // Shaped node with five rects, one of them outside the window.
    add(
        &mut store,
        &mut windows,
        0x300,
        400,
        40,
        120,
        90,
        None,
        true,
    );
    core.shape_bounding.insert(
        0x300,
        vec![
            xfixes::RegionRect {
                x: 0,
                y: 0,
                width: 120,
                height: 10,
            },
            xfixes::RegionRect {
                x: 0,
                y: 10,
                width: 10,
                height: 70,
            },
            xfixes::RegionRect {
                x: 110,
                y: 10,
                width: 10,
                height: 70,
            },
            xfixes::RegionRect {
                x: 0,
                y: 80,
                width: 120,
                height: 10,
            },
            xfixes::RegionRect {
                x: 100,
                y: 85,
                width: 60,
                height: 30,
            },
        ],
    );
    // Child extending beyond a tiny parent (the fvwm holding-window case),
    // plus a grandchild that is clipped away entirely.
    add(
        &mut store,
        &mut windows,
        0x400,
        600,
        300,
        10,
        10,
        None,
        true,
    );
    add(
        &mut store,
        &mut windows,
        0x401,
        -5,
        -5,
        100,
        100,
        Some(0x400),
        true,
    );
    add(
        &mut store,
        &mut windows,
        0x402,
        50,
        50,
        20,
        20,
        Some(0x401),
        true,
    );
    // Straddling the output's top-left corner, and fully off-output.
    add(
        &mut store,
        &mut windows,
        0x500,
        -50,
        -50,
        100,
        100,
        None,
        true,
    );
    add(
        &mut store,
        &mut windows,
        0x501,
        5000,
        5000,
        10,
        10,
        None,
        true,
    );
    // Manual-redirected top-level with (a) an automatic-redirected child
    // owning its own backing and (b) a plain child whose paint lands in
    // the manual ancestor's backing.
    add(
        &mut store,
        &mut windows,
        0x600,
        50,
        400,
        200,
        100,
        None,
        true,
    );
    add(
        &mut store,
        &mut windows,
        0x601,
        10,
        10,
        60,
        40,
        Some(0x600),
        true,
    );
    add(
        &mut store,
        &mut windows,
        0x602,
        100,
        10,
        60,
        40,
        Some(0x600),
        true,
    );
    let m_id = store.lookup(0x600).expect("manual present");
    let m_backing = alloc_backing(&mut store, 0xB600, 200, 100);
    store.set_redirected_target(m_id, Some(m_backing));
    store.set_scene_participating(m_id, false);
    let a_id = store.lookup(0x601).expect("automatic present");
    let a_backing = alloc_backing(&mut store, 0xB601, 60, 40);
    store.set_redirected_target(a_id, Some(a_backing));
    // Automatic-redirected top-level (sampled through its backing).
    add(
        &mut store,
        &mut windows,
        0x700,
        300,
        400,
        80,
        60,
        None,
        true,
    );
    let r_id = store.lookup(0x700).expect("automatic top present");
    let r_backing = alloc_backing(&mut store, 0xB700, 80, 60);
    store.set_redirected_target(r_id, Some(r_backing));
    // COW top-level with a stage child — alpha_passthrough subtree.
    add(&mut store, &mut windows, 0x800, 0, 0, 800, 600, None, true);
    add(
        &mut store,
        &mut windows,
        0x801,
        0,
        0,
        800,
        600,
        Some(0x800),
        true,
    );
    // A window with geometry but no storage at all.
    windows.insert(
        0x900,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 700,
            y: 500,
            width: 40,
            height: 40,
            depth: 24,
            mapped: true,
            viewable: true,
            parent: None,
            stack_rank: rank,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );

    core.top_level_order = vec![
        0x100, 0x200, 0x201, 0x300, 0x400, 0x500, 0x501, 0x600, 0x700, 0x900, 0x800,
    ];
    (core, store, windows)
}

#[test]
fn refactored_emitter_matches_the_legacy_emitter_exactly() {
    for layout in [
        (0, 0, 800u32, 600u32),
        (100, 50, 2560, 1440),
        (-300, -200, 800, 600),
        (123, 45, 640, 480),
    ] {
        for cow in [None, Some(0x800u32)] {
            let (core, mut store, windows) = differential_fixture();
            let legacy = walk_with(true, &core, &mut store, &windows, layout, cow);
            let new = walk_with(false, &core, &mut store, &windows, layout, cow);
            assert!(
                !legacy.draws.is_empty(),
                "fixture sanity: the tree must emit something at layout {layout:?}"
            );
            assert_eq!(new, legacy, "layout {layout:?} cow {cow:?}");
        }
    }
}

/// The fixture exercises the gates it claims to: count what each case
/// contributes so a silent no-op fixture cannot pass the test above.
#[test]
fn differential_fixture_exercises_every_gate() {
    let (core, mut store, windows) = differential_fixture();
    let out = walk_with(
        false,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        Some(0x800),
    );
    let views: Vec<u64> = out.draws.iter().map(|d| d.view).collect();
    let has = |xid: u32| views.contains(&(u64::from(xid) | 0xFF00_0000));
    let has_backing = |xid: u32| views.contains(&(u64::from(xid) | 0xB000_0000));
    assert!(
        has(0x100) && has(0x101) && has(0x102),
        "nesting emits all three"
    );
    assert!(!has(0x103) && !has(0x104), "unmapped subtree emits nothing");
    assert!(has(0x200) && has(0x201), "overlapping siblings both emit");
    assert_eq!(
        out.draws
            .iter()
            .filter(|d| d.view == u64::from(0x300u32) | 0xFF00_0000)
            .count(),
        5,
        "shaped node emits one draw per rect (the fifth clamped, not dropped)"
    );
    assert!(has(0x401), "oversized child emits, clipped to its parent");
    assert!(
        !has(0x402),
        "grandchild clipped away entirely emits nothing"
    );
    assert!(has(0x500), "straddling window emits");
    assert!(!has(0x501), "off-output window does not emit");
    assert!(
        !has(0x600) && !has_backing(0xB600),
        "manual-redirected node never emits"
    );
    assert!(
        has_backing(0xB601),
        "automatic child under a manual ancestor emits its backing"
    );
    assert!(
        !has(0x602),
        "plain child under a manual ancestor paints into the ancestor"
    );
    assert!(
        has_backing(0xB700) && !has(0x700),
        "automatic top-level samples its backing"
    );
    assert!(
        out.draws.iter().filter(|d| d.alpha_passthrough).count() == 2,
        "exactly the COW and its stage are alpha_passthrough"
    );
    assert!(!has(0x900), "geometry without storage emits nothing");
}

// ── Step 1 stage A: opaque cover is a union, tested by subtraction ────

#[test]
fn opaque_cover_accepts_a_union_of_draws_across_a_window_edge() {
    // Root fragmented around a window at (200,150)-(400,350), plus the window.
    let draws = [
        draw_at(0.0, 0.0, 800.0, 150.0, false),     // above
        draw_at(0.0, 350.0, 800.0, 250.0, false),   // below
        draw_at(0.0, 150.0, 200.0, 200.0, false),   // left
        draw_at(400.0, 150.0, 400.0, 200.0, false), // right
        draw_at(200.0, 150.0, 200.0, 200.0, false), // the window
    ];
    assert!(opaque_cover_exists(&draws, audit_rect(150, 100, 100, 100)));
    assert!(opaque_cover_exists(&draws, audit_rect(0, 0, 800, 600)));
}

#[test]
fn opaque_cover_rejects_a_one_pixel_gap_in_the_union() {
    let draws = [
        draw_at(0.0, 0.0, 800.0, 150.0, false),
        draw_at(0.0, 351.0, 800.0, 249.0, false), // leaves row 350 uncovered
        draw_at(0.0, 150.0, 200.0, 200.0, false),
        draw_at(400.0, 150.0, 400.0, 200.0, false),
        draw_at(200.0, 150.0, 200.0, 200.0, false),
    ];
    assert!(!opaque_cover_exists(&draws, audit_rect(150, 300, 100, 100)));
    // A translucent draw over the gap does not close it.
    let mut with_alpha = draws.to_vec();
    with_alpha.push(draw_at(0.0, 340.0, 800.0, 20.0, true));
    assert!(!opaque_cover_exists(
        &with_alpha,
        audit_rect(150, 300, 100, 100)
    ));
}

#[test]
fn clipped_path_with_two_scissors_covered_by_different_draws() {
    // No root: two opaque windows, each covering one damage rect.
    let draws = [
        draw_at(0.0, 0.0, 400.0, 600.0, false),
        draw_at(400.0, 0.0, 400.0, 600.0, false),
    ];
    // Two small rects far apart so the bbox is wasteful and 4.5 splits.
    let damage = region_of(&[audit_rect(10, 10, 20, 20), audit_rect(700, 500, 20, 20)]);
    let plan = plan_repaint(&damage, &draws, extent(800, 600), true, true);
    assert!(plan.full_reason.is_none(), "{:?}", plan.full_reason);
    assert_eq!(plan.scissors.len(), 2);
    assert!(
        plan.scissors
            .iter()
            .all(|r| opaque_cover_exists(&draws, *r))
    );
}

// ── Step 1 stage A: an incomplete submit never stages `painted` ──────

/// After an `invalidate` the model owes a full repaint; a tick that finds no
/// producer damage must compose anyway, not take the EmptyDamage skip.
#[test]
fn an_owed_repaint_is_not_an_empty_damage_skip() {
    assert!(skip_for_empty_damage(true, false, false), "idle skips");
    assert!(
        !skip_for_empty_damage(false, false, false),
        "damage composes"
    );
    assert!(
        !skip_for_empty_damage(true, true, false),
        "first frame composes"
    );
    assert!(
        !skip_for_empty_damage(true, false, true),
        "owed repaint (invalidated BO) must compose"
    );
}

#[test]
fn incomplete_submit_invalidates_instead_of_staging() {
    let ext = extent(800, 600);
    let mut damage = ScanoutDamage::new(2, ext);
    let repaint = region_of(&[audit_rect(10, 10, 40, 40)]);
    // Complete: staged as usual.
    stage_submitted_frame(&mut damage, true, 0, &repaint, &repaint);
    assert!(damage.has_staged_frame());
    damage.retire_success();
    assert!(!damage.has_staged_frame());
    // Incomplete: nothing staged, and every BO owes the whole output again.
    stage_submitted_frame(&mut damage, false, 1, &repaint, &repaint);
    assert!(!damage.has_staged_frame());
    for bo in 0..2 {
        assert_eq!(
            damage.missing_area(bo),
            u64::from(ext.width) * u64::from(ext.height),
            "bo {bo} must owe the full output after an incomplete submit"
        );
    }
}

// ── Step 1 stage B: the visibility walk ──────────────────────────────

/// One rasterised pixel: what the compose would show there, as the stack
/// of (view, source u, source v) samples an alpha draw leaves and an
/// opaque draw resets. Comparing stacks pixel for pixel between the
/// `On` and `Off` scenes is the invariant step 1 must keep: clipping
/// changes what is *drawn*, never what is *shown*.
type PixelStack = Vec<(u64, f64, f64)>;

fn rasterise(draws: &[CompositeDraw], w: u32, h: u32) -> Vec<PixelStack> {
    let (wi, hi) = (w as usize, h as usize);
    let mut grid: Vec<PixelStack> = vec![Vec::new(); wi * hi];
    for d in draws {
        let x0 = d.dst_origin[0].floor().max(0.0) as usize;
        let y0 = d.dst_origin[1].floor().max(0.0) as usize;
        let x1 = ((d.dst_origin[0] + d.dst_size[0]).ceil().max(0.0) as usize).min(wi);
        let y1 = ((d.dst_origin[1] + d.dst_size[1]).ceil().max(0.0) as usize).min(hi);
        let view = ash::vk::Handle::as_raw(d.image_view);
        for py in y0..y1 {
            for px in x0..x1 {
                let fx = (px as f64 + 0.5 - f64::from(d.dst_origin[0])) / f64::from(d.dst_size[0]);
                let fy = (py as f64 + 0.5 - f64::from(d.dst_origin[1])) / f64::from(d.dst_size[1]);
                let u = f64::from(d.src_origin[0]) + fx * f64::from(d.src_size[0]);
                let v = f64::from(d.src_origin[1]) + fy * f64::from(d.src_size[1]);
                let cell = &mut grid[py * wi + px];
                if d.alpha_passthrough {
                    cell.push((view, u, v));
                } else {
                    cell.clear();
                    cell.push((view, u, v));
                }
            }
        }
    }
    grid
}

fn stacks_equal(a: &PixelStack, b: &PixelStack) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.0 == y.0 && (x.1 - y.1).abs() < 1e-4 && (x.2 - y.2).abs() < 1e-4)
}

/// Assert the `On` scene shows the same pixels as the `Off` scene of the
/// same fixture, on every pixel of the output. Returns the `On` build for
/// further assertions.
fn assert_oracle(
    core: &KmsCore,
    store: &mut DrawableStore,
    windows: &crate::kms::render::backend::WindowsMap,
    layout: (i32, i32, u32, u32),
    cow: Option<u32>,
    label: &str,
) -> SceneBuild {
    let off = build_with(Visibility::Off, core, store, windows, layout, cow);
    let on = build_with(Visibility::On, core, store, windows, layout, cow);
    let (w, h) = (layout.2, layout.3);
    let a = rasterise(&off.scene.draws, w, h);
    let b = rasterise(&on.scene.draws, w, h);
    for (i, (sa, sb)) in a.iter().zip(&b).enumerate() {
        assert!(
            stacks_equal(sa, sb),
            "{label}: pixel ({},{}) differs: off={sa:?} on={sb:?} (layout {layout:?}, cow {cow:?})",
            i % w as usize,
            i / w as usize,
        );
    }
    assert_eq!(
        on.stats.draws_emitted,
        u64::try_from(on.scene.draws.len()).unwrap(),
        "{label}: the stats count what was emitted"
    );
    on
}

/// Root drawable at the logical screen size, sampled through a sentinel view.
fn alloc_root(core: &KmsCore, store: &mut DrawableStore, w: u32, h: u32) {
    let mut storage = crate::kms::render::store::Storage::for_tests_null(
        extent(w, h),
        vk::Format::B8G8R8A8_UNORM,
    );
    let view: ash::vk::ImageView = ash::vk::Handle::from_raw(0x00A0_7000);
    storage.image_view = view;
    storage.sample_view = view;
    store
        .allocate(core.window_id, DrawableKind::Root, 24, true, storage)
        .expect("alloc root stub");
}

fn area_of(r: vk::Rect2D) -> u64 {
    u64::from(r.extent.width) * u64::from(r.extent.height)
}

fn draws_of(built: &SceneBuild, view_raw: u64) -> Vec<vk::Rect2D> {
    built
        .scene
        .draws
        .iter()
        .filter(|d| ash::vk::Handle::as_raw(d.image_view) == view_raw)
        .filter_map(draw_dst_rect_inward)
        .collect()
}

fn win_view(xid: u32) -> u64 {
    u64::from(xid) | 0xFF00_0000
}

/// The reversal proof with a root present: under `Visibility::Off`,
/// `build_scene` produces exactly what the pre-step-1 code produced — the
/// root draw first, then the legacy emitter's list — bit for bit.
#[test]
fn off_mode_reproduces_the_legacy_root_and_emitter_exactly() {
    for layout in [
        (0, 0, 800u32, 600u32),
        (2560, 0, 2560, 1440),
        (-300, -200, 800, 600),
    ] {
        for cow in [None, Some(0x800u32)] {
            let (core, mut store, windows) = differential_fixture();
            alloc_root(&core, &mut store, 5120, 1440);
            let legacy = walk_with(true, &core, &mut store, &windows, layout, cow);
            let off = build_with(Visibility::Off, &core, &mut store, &windows, layout, cow);
            // The legacy root draw, as the old `build_scene` pushed it.
            let root_key = DrawKey {
                view: 0x00A0_7000,
                dst_origin: [(-layout.0) as f32, (-layout.1) as f32].map(f32::to_bits),
                dst_size: [5120.0f32, 1440.0f32].map(f32::to_bits),
                src_origin: [0.0f32, 0.0f32].map(f32::to_bits),
                src_size: [1.0f32, 1.0f32].map(f32::to_bits),
                alpha_passthrough: false,
            };
            let mut expect_draws = vec![root_key];
            expect_draws.extend(legacy.draws);
            let got = walk_out_of(&off);
            assert_eq!(
                got.draws, expect_draws,
                "draws, layout {layout:?} cow {cow:?}"
            );
            assert_eq!(
                got.participants.len(),
                legacy.participants.len() + 1,
                "one root presence plus the legacy ones"
            );
            assert_eq!(got.participants[0].id.role, SceneRole::Root);
            assert_eq!(&got.participants[1..], &legacy.participants[..]);
            assert_eq!(got.sampled.len(), legacy.sampled.len() + 1);
            assert_eq!(&got.sampled[1..], &legacy.sampled[..]);
            assert_eq!(got.stats_free_snapshots(), legacy.snapshots.len() + 1);
        }
    }
}

impl WalkOut {
    fn stats_free_snapshots(&self) -> usize {
        self.snapshots.len()
    }
}

/// The pixel oracle over the whole differential fixture, with a root, on
/// several layouts, with and without the COW subtree.
#[test]
fn visibility_shows_the_same_pixels_as_the_unclipped_scene() {
    for layout in [
        (0, 0, 800u32, 600u32),
        (100, 50, 700, 500),
        (-300, -200, 800, 600),
        (2560, 0, 640, 480),
    ] {
        for cow in [None, Some(0x800u32)] {
            let (core, mut store, windows) = differential_fixture();
            alloc_root(&core, &mut store, 5120, 1440);
            let on = assert_oracle(&core, &mut store, &windows, layout, cow, "fixture");
            assert!(on.stats.nodes_visited > 0);
            assert!(
                on.stats.draws_emitted == u64::try_from(on.scene.draws.len()).unwrap(),
                "stats count what was emitted"
            );
        }
    }
}

fn two_windows(
    lower: (i16, i16, u16, u16),
    upper: (i16, i16, u16, u16),
) -> (
    KmsCore,
    DrawableStore,
    crate::kms::render::backend::WindowsMap,
) {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_root(&core, &mut store, 800, 600);
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x100,
        lower.0,
        lower.1,
        lower.2,
        lower.3,
        None,
        true,
    );
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x200,
        upper.0,
        upper.1,
        upper.2,
        upper.3,
        None,
        true,
    );
    set_rank(&mut windows, 0x100, 1);
    set_rank(&mut windows, 0x200, 2);
    core.top_level_order = vec![0x100, 0x200];
    (core, store, windows)
}

/// An opaque top-level fully covering a lower one: the lower emits nothing
/// but is still a participant; the root emits the output minus the cover.
#[test]
fn a_fully_covered_window_emits_nothing_and_stays_a_participant() {
    let (core, mut store, windows) = two_windows((100, 100, 50, 50), (80, 80, 100, 100));
    let on = assert_oracle(
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
        "full cover",
    );
    assert!(
        draws_of(&on, win_view(0x100)).is_empty(),
        "covered window emits nothing"
    );
    assert_eq!(draws_of(&on, win_view(0x200)).len(), 1);
    let root: u64 = draws_of(&on, 0x00A0_7000).iter().map(|r| area_of(*r)).sum();
    assert_eq!(
        root,
        800 * 600 - 100 * 100,
        "root = output − the opaque cover"
    );
    assert_eq!(on.stats.hidden_participants, 1);
    let hidden = on
        .participants
        .iter()
        .find(|p| p.id.xid == 0x100)
        .expect("hidden window is still a participant");
    assert!(hidden.visible.is_empty());
    assert_eq!(
        hidden.region.bounding_rect(),
        Some(audit_rect(100, 100, 50, 50))
    );
    // Placement, not visibility: the presence region is the full window.
    assert_eq!(hidden.region.area(), 50 * 50);
}

/// Partial cover: the lower window emits only its visible pieces.
#[test]
fn a_partly_covered_window_emits_only_its_visible_pieces() {
    let (core, mut store, windows) = two_windows((100, 100, 200, 200), (250, 150, 200, 200));
    let on = assert_oracle(
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
        "partial",
    );
    let lower: u64 = draws_of(&on, win_view(0x100))
        .iter()
        .map(|r| area_of(*r))
        .sum();
    // 200×200 minus the 50×150 overlap.
    assert_eq!(lower, 200 * 200 - 50 * 150);
    assert!(
        draws_of(&on, win_view(0x100)).len() > 1,
        "emitted as pieces"
    );
    assert_eq!(on.stats.hidden_participants, 0);
}

/// The parent-bounding-shape fix, written before the walk: an EMPTY parent
/// shape suppresses its children; a partial shape clips them. Under the
/// pre-step-1 emitter the child clip came from the parent's rect alone.
#[test]
fn an_empty_parent_shape_suppresses_its_children() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_root(&core, &mut store, 800, 600);
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x100,
        100,
        100,
        200,
        200,
        None,
        true,
    );
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x101,
        10,
        10,
        50,
        50,
        Some(0x100),
        true,
    );
    core.top_level_order = vec![0x100];
    core.shape_bounding.insert(0x100, Vec::new());
    let on = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    assert!(
        draws_of(&on, win_view(0x100)).is_empty(),
        "empty shape: parent draws nothing"
    );
    assert!(
        draws_of(&on, win_view(0x101)).is_empty(),
        "…and neither do its children"
    );
    let root: u64 = draws_of(&on, 0x00A0_7000).iter().map(|r| area_of(*r)).sum();
    assert_eq!(root, 800 * 600, "the root shows through the whole hole");
}

#[test]
fn a_partial_parent_shape_clips_its_children() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_root(&core, &mut store, 800, 600);
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x100,
        100,
        100,
        200,
        200,
        None,
        true,
    );
    // Child spans the whole parent; the parent's shape is its left half.
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x101,
        0,
        0,
        200,
        200,
        Some(0x100),
        true,
    );
    core.top_level_order = vec![0x100];
    core.shape_bounding.insert(
        0x100,
        vec![xfixes::RegionRect {
            x: 0,
            y: 0,
            width: 100,
            height: 200,
        }],
    );
    let on = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    let child = draws_of(&on, win_view(0x101));
    let child_area: u64 = child.iter().map(|r| area_of(*r)).sum();
    assert_eq!(child_area, 100 * 200, "child clipped to the parent's shape");
    assert!(
        child
            .iter()
            .all(|r| r.offset.x + i32::try_from(r.extent.width).unwrap() <= 200),
        "no child pixel outside the shaped half: {child:?}"
    );
    // The parent is entirely under its child within the shape: nothing left.
    assert!(draws_of(&on, win_view(0x100)).is_empty());
    let root: u64 = draws_of(&on, 0x00A0_7000).iter().map(|r| area_of(*r)).sum();
    assert_eq!(root, 800 * 600 - 100 * 200);
}

/// COW subtree above opaque windows: the COW claims nothing, so the windows
/// below still emit in full, and the COW blends over them.
#[test]
fn a_cow_subtree_claims_nothing() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_root(&core, &mut store, 800, 600);
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x100,
        100,
        100,
        200,
        200,
        None,
        true,
    );
    alloc_stub_window(&mut store, &mut windows, 0x800, 0, 0, 800, 600, None, true);
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x801,
        0,
        0,
        800,
        600,
        Some(0x800),
        true,
    );
    set_rank(&mut windows, 0x100, 1);
    set_rank(&mut windows, 0x800, 2);
    core.top_level_order = vec![0x100, 0x800];
    let on = assert_oracle(
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        Some(0x800),
        "cow",
    );
    assert_eq!(
        draws_of(&on, win_view(0x100)).len(),
        1,
        "window under the COW emits whole"
    );
    let root: u64 = draws_of(&on, 0x00A0_7000).iter().map(|r| area_of(*r)).sum();
    assert_eq!(
        root,
        800 * 600 - 200 * 200,
        "root loses only the opaque window"
    );
    assert_eq!(on.stats.hidden_participants, 0);
}

/// A manual-redirected top-level emits nothing itself but its opaque
/// automatic child claims through it: the root loses the child's area.
#[test]
fn an_opaque_automatic_child_claims_through_a_manual_parent() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_root(&core, &mut store, 800, 600);
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x600,
        50,
        400,
        200,
        100,
        None,
        true,
    );
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x601,
        10,
        10,
        60,
        40,
        Some(0x600),
        true,
    );
    let m_id = store.lookup(0x600).unwrap();
    let m_backing = alloc_backing(&mut store, 0xB600, 200, 100);
    store.set_redirected_target(m_id, Some(m_backing));
    store.set_scene_participating(m_id, false);
    let a_id = store.lookup(0x601).unwrap();
    let a_backing = alloc_backing(&mut store, 0xB601, 60, 40);
    store.set_redirected_target(a_id, Some(a_backing));
    core.top_level_order = vec![0x600];
    let on = assert_oracle(
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
        "manual",
    );
    let backing_view = u64::from(0xB601u32) | 0xB000_0000;
    assert_eq!(draws_of(&on, backing_view).len(), 1);
    let root = draws_of(&on, 0x00A0_7000);
    let root_area: u64 = root.iter().map(|r| area_of(*r)).sum();
    assert_eq!(
        root_area,
        800 * 600 - 60 * 40,
        "root loses exactly the child's area"
    );
    assert!(
        root.iter()
            .all(|r| !rects_intersect(*r, audit_rect(60, 410, 60, 40))),
        "no root piece under the automatic child"
    );
}

/// Straddling window and non-zero layout origin: pieces sample the same
/// texels as the unclipped draw (checked by the oracle) and lie on the
/// output.
#[test]
fn straddling_windows_and_layout_origins_sample_the_right_texels() {
    let (core, mut store, windows) = two_windows((-50, -50, 100, 100), (700, 500, 200, 200));
    let on = assert_oracle(
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
        "straddle",
    );
    for d in &on.scene.draws {
        let r = draw_dst_rect_inward(d).unwrap();
        assert!(r.offset.x >= 0 && r.offset.y >= 0, "output-clipped: {r:?}");
        assert!(
            r.offset.x + i32::try_from(r.extent.width).unwrap() <= 800
                && r.offset.y + i32::try_from(r.extent.height).unwrap() <= 600,
            "output-clipped: {r:?}"
        );
    }
    // The top-left straddler shows its bottom-right quarter: src starts at 0.5.
    let piece = on
        .scene
        .draws
        .iter()
        .find(|d| ash::vk::Handle::as_raw(d.image_view) == win_view(0x100))
        .expect("straddler emits");
    assert_eq!(piece.dst_origin, [0.0, 0.0]);
    assert_eq!(piece.src_origin, [0.5, 0.5]);
    assert_eq!(piece.src_size, [0.5, 0.5]);
    // Same tree on the second output of a side-by-side layout.
    let (core, mut store, windows) = two_windows((2500, 100, 120, 100), (2700, 300, 50, 50));
    let on = assert_oracle(
        &core,
        &mut store,
        &windows,
        (2560, 0, 640, 480),
        None,
        "x0=2560",
    );
    let piece = on
        .scene
        .draws
        .iter()
        .find(|d| ash::vk::Handle::as_raw(d.image_view) == win_view(0x100))
        .expect("emits");
    // Window at logical x=2500 is 60px off the left edge of this output.
    assert_eq!(piece.dst_origin, [0.0, 100.0]);
    assert_eq!(piece.src_origin, [0.5, 0.0]);
    assert_eq!(piece.src_size, [0.5, 1.0]);
}

/// A redirected window whose backing outgrew it (a shrink keeps the old
/// backing): `src` divides by the SAMPLED source's extent, so the piece
/// samples the window's texels at the backing's origin rather than
/// stretching the whole backing. The pre-step-1 emitter divided by the
/// host size — kept under `Off` only — so this case has no oracle and is
/// asserted directly.
#[test]
fn a_redirected_backing_larger_than_its_host_is_sampled_unstretched() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_root(&core, &mut store, 800, 600);
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x700,
        100,
        100,
        80,
        60,
        None,
        true,
    );
    let r_id = store.lookup(0x700).unwrap();
    let backing = alloc_backing(&mut store, 0xB700, 160, 120); // 2× the host
    store.set_redirected_target(r_id, Some(backing));
    core.top_level_order = vec![0x700];
    let on = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    let d = on
        .scene
        .draws
        .iter()
        .find(|d| ash::vk::Handle::as_raw(d.image_view) == (u64::from(0xB700u32) | 0xB000_0000))
        .expect("redirected window emits its backing");
    assert_eq!(d.dst_origin, [100.0, 100.0]);
    assert_eq!(d.dst_size, [80.0, 60.0]);
    assert_eq!(d.src_origin, [0.0, 0.0]);
    assert_eq!(
        d.src_size,
        [0.5, 0.5],
        "80/160 × 60/120: the host's texels only"
    );
    // And the presence signature agrees with the unclipped draw.
    let p = on.participants.iter().find(|p| p.id.xid == 0x700).unwrap();
    assert_eq!(
        p.signature,
        PresenceSignature::new(d.image_view, d.src_origin, d.src_size, false)
    );
}

/// More than 32 opaque fragments over the root: the root's universe
/// collapses to its bounding box (a superset), so the root over-emits —
/// and the oracle still holds, because painter's order repaints the extra.
#[test]
fn a_collapsed_universe_over_emits_but_shows_the_same_pixels() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_root(&core, &mut store, 800, 600);
    let mut rank = 1;
    for i in 0..7i16 {
        for j in 0..7i16 {
            let xid = 0x1000 + u32::try_from(i * 7 + j).unwrap();
            alloc_stub_window(
                &mut store,
                &mut windows,
                xid,
                20 + i * 100,
                20 + j * 70,
                40,
                30,
                None,
                true,
            );
            set_rank(&mut windows, xid, rank);
            rank += 1;
            core.top_level_order.push(xid);
        }
    }
    let on = assert_oracle(
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
        "collapse",
    );
    assert!(
        on.stats.collapses() > 0,
        "49 disjoint claims must overflow the 32-box cap"
    );
    let root: u64 = draws_of(&on, 0x00A0_7000).iter().map(|r| area_of(*r)).sum();
    assert!(
        root > 800 * 600 - 49 * 40 * 30,
        "root over-emits after the collapse (superset), never under"
    );
    assert!(root <= 800 * 600);
}

/// Step 2 under step 1: a window hidden by an unrelated move above it
/// contributes no structural damage of its own — the mover's old ∪ new
/// covers it — and its rank is unchanged, so nothing reads as restacked.
#[test]
fn hiding_a_window_by_moving_another_over_it_damages_only_the_mover() {
    // Frame 1: B beside A. Frame 2: B moved onto A, covering it entirely.
    let (core, mut store, windows) = two_windows((100, 100, 50, 50), (400, 400, 100, 100));
    let before = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    let (core2, mut store2, windows2) = two_windows((100, 100, 50, 50), (80, 80, 100, 100));
    let after = build_with(
        Visibility::On,
        &core2,
        &mut store2,
        &windows2,
        (0, 0, 800, 600),
        None,
    );
    // Participant identity uses the store generation; both fixtures allocate
    // in the same order so the ids line up.
    assert_eq!(
        before.participants.iter().map(|p| p.id).collect::<Vec<_>>(),
        after.participants.iter().map(|p| p.id).collect::<Vec<_>>(),
        "same participants in the same (painter's) order — nothing restacked, \
             and the hidden window is still listed"
    );
    let damage = structural_damage(&before.participants, &after.participants);
    let mut expect = Region::from_rect(audit_rect(400, 400, 100, 100));
    expect.union_with(&Region::from_rect(audit_rect(80, 80, 100, 100)));
    assert_eq!(damage, expect, "exactly the mover's old ∪ new");
    assert!(
        after
            .participants
            .iter()
            .any(|p| p.id.xid == 0x100 && p.visible.is_empty()),
        "the covered window is present with an empty visible region"
    );
}

/// A pure restack of two overlapping top-levels owes only their overlap: the
/// step-2 rule damages pairwise intersections of participants whose relative
/// order flipped, not the whole region of everything whose rank moved.
#[test]
fn swapping_two_overlapping_top_levels_damages_only_their_overlap() {
    let lower = (100i16, 100i16, 300u16, 300u16);
    let upper = (250i16, 250i16, 300u16, 300u16);
    let (core, mut store, windows) = two_windows(lower, upper);
    let before = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    let (mut core2, mut store2, mut windows2) = two_windows(lower, upper);
    // Swap the stacking: 0x200 now below 0x100.
    set_rank(&mut windows2, 0x100, 2);
    set_rank(&mut windows2, 0x200, 1);
    core2.top_level_order = vec![0x200, 0x100];
    let after = build_with(
        Visibility::On,
        &core2,
        &mut store2,
        &windows2,
        (0, 0, 800, 600),
        None,
    );
    let damage = structural_damage(&before.participants, &after.participants);
    let overlap = Region::from_rect(audit_rect(250, 250, 150, 150));
    assert_eq!(damage, overlap, "exactly lower ∩ upper");
}

/// The `Off` scene keeps `visible == region` for every participant, so the
/// audit's reference carries the same presences as before step 1.
#[test]
fn off_mode_presences_are_fully_visible() {
    let (core, mut store, windows) = differential_fixture();
    let off = build_with(
        Visibility::Off,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    for p in &off.participants {
        assert_eq!(p.visible, p.region, "{:?}", p.id);
    }
    assert_eq!(off.stats.hidden_participants, 0);
    assert_eq!(off.stats.collapses(), 0);
}

// ── Step 1 stage C: content damage clipped to visibility ─────────────

fn drawable_of(store: &DrawableStore, xid: u32) -> crate::kms::render::store::DrawableId {
    store.lookup(xid).expect("fixture window has a drawable")
}

fn projected_sorted(built: &SceneBuild) -> Vec<vk::Rect2D> {
    sorted_rects(built.projected_damage.rects().to_vec())
}

/// A paint into the covered part of a window changes no pixel on screen:
/// it projects nothing, classifies `Hidden`, and must not force a compose.
/// The snapshot is still carried (it acks if something else composes).
#[test]
fn hidden_paint_projects_nothing_and_does_not_force() {
    // Lower 0x100 fully under upper 0x200.
    let (core, mut store, windows) = two_windows((100, 100, 50, 50), (80, 80, 100, 100));
    let lower = drawable_of(&store, 0x100);
    store.damage(lower, rect(5, 5, 20, 20));
    let built = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    assert!(
        built.projected_damage.is_empty(),
        "hidden paint projected {:?}",
        built.projected_damage.rects()
    );
    assert_eq!(built.stats.content_hidden, 1);
    assert_eq!(built.stats.content_off_output, 0);
    assert_eq!(built.stats.content_visible, 0);
    assert!(
        !built.stats.off_output_damage_forces_compose(),
        "hidden damage must take the EmptyDamage skip, not force a Full compose"
    );
    assert!(
        !built.snapshots.iter().any(|s| s.id == lower),
        "a hidden snapshot must NOT ride this output's build: it was not presented \
             here, and carrying it would let this output's retire ack it globally \
             (the multi-output ack race, 2026-09-04)"
    );
    // Un-acked: the store still holds it for the next walk.
    assert!(
        !store
            .peek_presentation_damage(lower)
            .unwrap()
            .region
            .is_empty()
    );
}

/// Codex, post-merge review of `02bafec3` (finding 1): a drawable whose
/// damage classified `Hidden` was still counted as drawn, so
/// `reconcile_offscreen_no_draw` never flagged it and
/// `has_pending_presentation_damage` kept waking the tick — ~1850 walks/s
/// at 2 composes/s with mpv under a terminal. The presented set must leave
/// it out; a drawable with visible damage stays in.
#[test]
fn hidden_damage_is_not_presented_so_the_scheduler_can_go_dormant() {
    // Lower 0x100 fully under upper 0x200; both painted.
    let (core, mut store, windows) = two_windows((100, 100, 50, 50), (80, 80, 100, 100));
    let lower = drawable_of(&store, 0x100);
    let upper = drawable_of(&store, 0x200);
    store.damage(lower, rect(5, 5, 20, 20));
    store.damage(upper, rect(1, 1, 5, 5));
    let built = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    assert_eq!(built.stats.content_hidden, 1);
    assert!(
        built.sampled_ids.contains(&lower),
        "sampling bookkeeping is unchanged: the hidden node is still a participant"
    );
    assert!(
        !built.presented_ids.contains(&lower),
        "hidden damage was not presented: {:?}",
        built.presented_ids
    );
    assert!(built.presented_ids.contains(&upper));
    assert!(
        !built.pieces_ids.contains(&lower),
        "fully covered: no pieces either ⇒ NoPieces, stays dormant across paints"
    );
    let drawn: std::collections::HashSet<_> = built.presented_ids.iter().copied().collect();
    let pieces: std::collections::HashSet<_> = built.pieces_ids.iter().copied().collect();
    store.reconcile_offscreen_no_draw(&drawn, &pieces);
    assert_eq!(
        store.get(lower).unwrap().dormant,
        Some(crate::kms::render::store::DormantReason::NoPieces),
        "flagged out of the scheduler"
    );
    assert!(store.get(upper).unwrap().dormant.is_none());
    assert!(
        !store
            .peek_presentation_damage(lower)
            .unwrap()
            .region
            .is_empty(),
        "damage is preserved, only the flag changes"
    );

    // A PARTIALLY covered node whose damage lies entirely under the cover:
    // it emits pieces (it is drawn) but presented nothing of its paint.
    let (core, mut store, windows) = two_windows((100, 100, 200, 200), (200, 100, 200, 200));
    let lower = drawable_of(&store, 0x100);
    // Storage-local x 150..190 → output x 250..290, under the cover (x ≥ 200).
    store.damage(lower, rect(150, 50, 40, 20));
    let built = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    assert_eq!(built.stats.content_hidden, 1);
    assert!(built.stats.draws_emitted > 0);
    assert!(built.sampled_ids.contains(&lower));
    assert!(!built.presented_ids.contains(&lower));
    assert!(
        built.pieces_ids.contains(&lower),
        "partially covered: it emitted pieces ⇒ HiddenDamage, re-armed by the next paint"
    );
    let drawn: std::collections::HashSet<_> = built.presented_ids.iter().copied().collect();
    let pieces: std::collections::HashSet<_> = built.pieces_ids.iter().copied().collect();
    store.reconcile_offscreen_no_draw(&drawn, &pieces);
    assert_eq!(
        store.get(lower).unwrap().dormant,
        Some(crate::kms::render::store::DormantReason::HiddenDamage)
    );
    assert!(
        !store.has_pending_presentation_damage(),
        "nothing presentable is pending: the scheduler must go dormant"
    );
    // The next paint lands in the VISIBLE part (storage-local x 10..40 →
    // output 110..140, left of the cover): it must re-arm and present.
    store.damage(lower, rect(10, 10, 30, 30));
    assert!(
        store.has_pending_presentation_damage(),
        "a paint into a HiddenDamage-dormant window re-arms the scheduler"
    );
    let built = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    assert_eq!(built.stats.content_visible, 1);
    assert!(built.presented_ids.contains(&lower));
}

/// Two outputs: hidden on output 0, visible on output 1. The union of the
/// two outputs' presented sets contains it, so the drawable stays armed and
/// output 1 composes it.
#[test]
fn damage_visible_on_one_output_keeps_the_drawable_armed() {
    let (core, mut store, windows) = two_windows((700, 100, 200, 100), (650, 0, 150, 600));
    let w = drawable_of(&store, 0x100);
    store.damage(w, rect(0, 0, 200, 100));
    let out0 = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    let out1 = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (800, 0, 800, 600),
        None,
    );
    assert_eq!(out0.stats.content_hidden, 1);
    assert!(!out0.presented_ids.contains(&w));
    assert_eq!(out1.stats.content_visible, 1);
    assert!(out1.presented_ids.contains(&w));
    let mut drawn: std::collections::HashSet<_> = out0.presented_ids.iter().copied().collect();
    drawn.extend(out1.presented_ids.iter().copied());
    let mut pieces: std::collections::HashSet<_> = out0.pieces_ids.iter().copied().collect();
    pieces.extend(out1.pieces_ids.iter().copied());
    store.reconcile_offscreen_no_draw(&drawn, &pieces);
    assert!(store.get(w).unwrap().dormant.is_none());
    assert!(store.has_pending_presentation_damage());
}

/// Paint straddling a cover's edge projects only the visible side.
#[test]
fn paint_across_a_cover_edge_projects_only_the_visible_side() {
    // Lower 0x100 at x 100..300; upper 0x200 covers x 200..400.
    let (core, mut store, windows) = two_windows((100, 100, 200, 200), (200, 100, 200, 200));
    let lower = drawable_of(&store, 0x100);
    // Storage-local (50,50)+(100x20) → output (150,150)+(100x20); visible x 150..200.
    store.damage(lower, rect(50, 50, 100, 20));
    let built = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    assert_eq!(projected_sorted(&built), vec![rect(150, 150, 50, 20)]);
    assert_eq!(built.stats.content_visible, 1);
    assert_eq!(built.stats.content_hidden, 0);
    // Paint entirely in the visible half projects whole.
    let (core, mut store, windows) = two_windows((100, 100, 200, 200), (200, 100, 200, 200));
    let lower = drawable_of(&store, 0x100);
    store.damage(lower, rect(10, 10, 30, 30));
    let built = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    assert_eq!(projected_sorted(&built), vec![rect(110, 110, 30, 30)]);
}

/// The xfce-submenu case is pinned: a paint whose projection misses the
/// output entirely still forces a compose, so the snapshot can ack.
#[test]
fn off_output_paint_still_forces_a_compose() {
    // 0x100 straddles the right edge: x 750..850 on an 800-wide output.
    let (core, mut store, windows) = two_windows((750, 100, 100, 50), (10, 10, 20, 20));
    let w = drawable_of(&store, 0x100);
    // Storage-local x 60..90 → output x 810..840: off the output.
    store.damage(w, rect(60, 5, 30, 10));
    let built = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    assert!(built.projected_damage.is_empty());
    assert_eq!(built.stats.content_off_output, 1);
    assert_eq!(built.stats.content_hidden, 0);
    assert!(built.stats.off_output_damage_forces_compose());
    // And a paint on the on-output part of the same window is Visible.
    let (core, mut store, windows) = two_windows((750, 100, 100, 50), (10, 10, 20, 20));
    let w = drawable_of(&store, 0x100);
    store.damage(w, rect(10, 5, 30, 10));
    let built = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    assert_eq!(projected_sorted(&built), vec![rect(760, 105, 30, 10)]);
    assert_eq!(built.stats.content_visible, 1);
    assert!(!built.stats.off_output_damage_forces_compose());
}

/// Hidden damage is not acked; it accumulates and shows when uncovered.
/// Frame 1: W paints under A (Hidden). Frame 2: A moves away — A's
/// structural old ∪ new covers W, and W's accumulated damage projects.
#[test]
fn uncovering_a_window_surfaces_its_accumulated_hidden_paint() {
    let (core, mut store, mut windows) = two_windows((100, 100, 50, 50), (80, 80, 100, 100));
    let lower = drawable_of(&store, 0x100);
    store.damage(lower, rect(5, 5, 20, 20));
    let frame1 = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    assert!(frame1.projected_damage.is_empty());
    assert_eq!(frame1.stats.content_hidden, 1);
    // Not acked (no compose happened): a second hidden paint accumulates.
    store.damage(lower, rect(30, 30, 10, 10));
    // Frame 2: A moves off W.
    let a = windows.get_mut(&0x200).expect("A");
    a.x = 400;
    a.y = 400;
    let frame2 = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    assert_eq!(
        projected_sorted(&frame2),
        vec![rect(105, 105, 20, 20), rect(130, 130, 10, 10)],
        "both accumulated paints project once W is visible"
    );
    assert_eq!(frame2.stats.content_visible, 1);
    let structural = structural_damage(&frame1.participants, &frame2.participants);
    assert!(
        structural.contains_rect(rect(100, 100, 50, 50)),
        "the mover's old ∪ new covers the uncovered window: {structural:?}"
    );
}

/// Two outputs, one store: W hidden on output 0 is visible on output 1.
/// Output 0 classifies Hidden and does not force; output 1 projects it. No
/// ack happens on the hidden side, which is what keeps output 1 correct.
#[test]
fn hidden_on_one_output_visible_on_the_other() {
    // W 0x100 at x 700..900 spans both outputs; A 0x200 covers x 650..800.
    let (core, mut store, windows) = two_windows((700, 100, 200, 100), (650, 0, 150, 600));
    let w = drawable_of(&store, 0x100);
    store.damage(w, rect(0, 0, 200, 100));
    let out0 = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    assert!(out0.projected_damage.is_empty());
    assert_eq!(out0.stats.content_hidden, 1);
    assert!(!out0.stats.off_output_damage_forces_compose());
    // Still in the store for output 1's walk.
    assert!(!store.peek_presentation_damage(w).unwrap().region.is_empty());
    let out1 = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (800, 0, 800, 600),
        None,
    );
    assert_eq!(projected_sorted(&out1), vec![rect(0, 100, 100, 100)]);
    assert_eq!(out1.stats.content_visible, 1);
    // Only the presenting output carries the snapshot into its PendingAck.
    assert!(
        !out0.snapshots.iter().any(|s| s.id == w),
        "output 0 (hidden) must not carry W's snapshot"
    );
    assert!(
        out1.snapshots
            .iter()
            .any(|s| s.id == w && !s.region.is_empty()),
        "output 1 (visible) carries it and acks it at its retire"
    );
}

// ── dormancy across outputs that did not walk ────────────────────────

fn set(ids: &[u64]) -> std::collections::HashSet<crate::kms::render::store::DrawableId> {
    ids.iter()
        .map(|i| crate::kms::render::store::DrawableId::for_tests(*i))
        .collect()
}

/// Hidden on output 0 (walked), but output 1 skipped this tick and its
/// retained pieces include the drawable: it may present it once it walks,
/// so it stays armed.
#[test]
fn dormancy_keeps_a_drawable_armed_when_a_skipped_output_may_present_it() {
    let none = set(&[]);
    let out0_pieces = set(&[7, 9]);
    let out0_presented = set(&[9]);
    let out1_last = set(&[7]);
    let reports = [
        OutputWalkReport {
            walked: true,
            presented: &out0_presented,
            last_pieces: &out0_pieces,
        },
        OutputWalkReport {
            walked: false,
            presented: &none,
            last_pieces: &out1_last,
        },
    ];
    let (keep_armed, pieces) = dormancy_inputs(&reports);
    assert!(keep_armed.contains(&set(&[7]).into_iter().next().unwrap()));
    assert!(keep_armed.contains(&set(&[9]).into_iter().next().unwrap()));
    assert!(pieces.contains(&set(&[7]).into_iter().next().unwrap()));
}

/// Same, but output 1 walked and did not present it either: dormant with
/// reason `HiddenDamage`, since it has pieces.
#[test]
fn dormancy_flags_hidden_damage_when_every_walked_output_declined_it() {
    let out0_pieces = set(&[7, 9]);
    let out0_presented = set(&[9]);
    let out1_pieces = set(&[7]);
    let out1_presented = set(&[]);
    let reports = [
        OutputWalkReport {
            walked: true,
            presented: &out0_presented,
            last_pieces: &out0_pieces,
        },
        OutputWalkReport {
            walked: true,
            presented: &out1_presented,
            last_pieces: &out1_pieces,
        },
    ];
    let (keep_armed, pieces) = dormancy_inputs(&reports);
    let seven = set(&[7]).into_iter().next().unwrap();
    assert!(!keep_armed.contains(&seven));
    assert!(pieces.contains(&seven), "⇒ HiddenDamage");
}

/// In no output's pieces, output 1 skipped: `NoPieces`.
#[test]
fn dormancy_flags_no_pieces_when_no_output_shows_it() {
    let out0_pieces = set(&[9]);
    let out0_presented = set(&[9]);
    let none = set(&[]);
    let out1_last = set(&[11]);
    let reports = [
        OutputWalkReport {
            walked: true,
            presented: &out0_presented,
            last_pieces: &out0_pieces,
        },
        OutputWalkReport {
            walked: false,
            presented: &none,
            last_pieces: &out1_last,
        },
    ];
    let (keep_armed, pieces) = dormancy_inputs(&reports);
    let seven = set(&[7]).into_iter().next().unwrap();
    assert!(!keep_armed.contains(&seven));
    assert!(!pieces.contains(&seven), "⇒ NoPieces");
    // And 11, shown only on the skipped output, stays armed.
    assert!(keep_armed.contains(&set(&[11]).into_iter().next().unwrap()));
}

/// The hardware case (silence/MATE 2026-09-04): the root's damage lies
/// under covers on both outputs; output 1 keeps skipping as NothingPending.
/// After output 0's walk alone the root must go dormant (`HiddenDamage`:
/// it has pieces on both), or it is re-peeked and re-classified Hidden
/// ~1000×/s forever.
#[test]
fn dormancy_runs_without_every_output_walking() {
    let root = set(&[1]);
    let out0_pieces = set(&[1, 5]);
    let out0_presented = set(&[5]);
    let none = set(&[]);
    let out1_last = set(&[1, 6]);
    let reports = [
        OutputWalkReport {
            walked: true,
            presented: &out0_presented,
            last_pieces: &out0_pieces,
        },
        OutputWalkReport {
            walked: false,
            presented: &none,
            last_pieces: &out1_last,
        },
    ];
    let (keep_armed, pieces) = dormancy_inputs(&reports);
    let one = root.into_iter().next().unwrap();
    // Output 1 has pieces for the root and did not walk ⇒ it MAY present
    // it ⇒ armed. That is the sound rule; what makes it terminate on
    // hardware is that output 1's predicate then walks (root armed and in
    // its last_pieces), declines it (Hidden), and the NEXT reconciliation
    // sees both outputs decline ⇒ dormant.
    assert!(keep_armed.contains(&one));
    let out1_pieces = set(&[1, 6]);
    let out1_presented = set(&[6]);
    let reports = [
        OutputWalkReport {
            walked: true,
            presented: &out0_presented,
            last_pieces: &out0_pieces,
        },
        OutputWalkReport {
            walked: true,
            presented: &out1_presented,
            last_pieces: &out1_pieces,
        },
    ];
    let (keep_armed, pieces2) = dormancy_inputs(&reports);
    assert!(!keep_armed.contains(&one));
    assert!(pieces2.contains(&one), "⇒ HiddenDamage");
    let _ = pieces;
}

/// The multi-output ack race, first half (silence/MATE, 2026-09-04): a
/// paint into a window spanning both outputs, landing inside output 0 only.
/// Output 1 knows (from output 0's retained pieces) that the window is shown
/// there, so it classifies `OtherOutput`: no force, no snapshot, not
/// presented — output 0 owns that damage. Before this rule output 1 read it
/// as `OffOutput`, forced a Full compose, and its retire acked the damage
/// before output 0 had composed it.
#[test]
fn other_output_damage_is_neither_forced_nor_carried_here() {
    // W 0x100 at x 700..900 spans the boundary at 800; 0x200 is far away.
    let (core, mut store, windows) = two_windows((700, 100, 200, 100), (0, 0, 10, 10));
    let w = drawable_of(&store, 0x100);
    // Storage-local x 0..50 → output-0 x 700..750 only.
    store.damage(w, rect(0, 0, 50, 100));
    let out0 = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    assert_eq!(out0.stats.content_visible, 1);
    assert!(
        out0.snapshots
            .iter()
            .any(|s| s.id == w && !s.region.is_empty())
    );
    assert!(out0.presented_ids.contains(&w));
    let elsewhere: std::collections::HashSet<_> = out0.pieces_ids.iter().copied().collect();
    let out1 = build_with_elsewhere(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (800, 0, 800, 600),
        None,
        &elsewhere,
    );
    assert_eq!(out1.stats.content_other_output, 1, "{:?}", out1.stats);
    assert_eq!(out1.stats.content_off_output, 0);
    assert!(
        !out1.stats.off_output_damage_forces_compose(),
        "damage another output presents must not force a Full compose here"
    );
    assert!(
        !out1.snapshots.iter().any(|s| s.id == w),
        "output 1 must not carry (and later ack) damage it did not present"
    );
    assert!(!out1.presented_ids.contains(&w));
    assert!(
        out1.pieces_ids.contains(&w),
        "W's right half is visible on output 1, so it has pieces there"
    );
    assert!(out1.projected_damage.is_empty());
}

/// The xfce-submenu rule, pinned: an off-output paint still FORCES a
/// compose here, so a paint whose projection is empty is not left
/// undrained. It is deliberately **not carried and not presented** — the
/// forced compose displays none of those pixels, and carrying the snapshot
/// is what let a cold `elsewhere` turn this branch into the multi-output
/// ack race (2026-09-04: caja's spanning desktop, 247 unhealed audit
/// mismatches). The drain comes from dormancy instead: not presented ⇒
/// dormant ⇒ no re-forcing until the next paint. A window entirely off the
/// output never reaches the classifier at all (the intersects gate), so the
/// fixture is a spanning window whose damage projects off output 0.
#[test]
fn damage_off_output_forces_a_compose_but_is_never_carried() {
    let (core, mut store, windows) = two_windows((700, 100, 200, 100), (0, 0, 10, 10));
    let w = drawable_of(&store, 0x100);
    // Storage-local x 150..200 → x 850..900: off output 0, on output 1.
    store.damage(w, rect(150, 0, 50, 100));
    let nowhere = std::collections::HashSet::new();
    let out0 = build_with_elsewhere(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
        &nowhere,
    );
    assert_eq!(out0.stats.content_off_output, 1, "{:?}", out0.stats);
    assert_eq!(out0.stats.content_other_output, 0);
    assert!(
        out0.stats.off_output_damage_forces_compose(),
        "an empty projection must still force a compose (xfce submenu)"
    );
    assert!(
        !out0.snapshots.iter().any(|s| s.id == w),
        "the forced compose shows none of those pixels, so it must not ack them"
    );
    assert!(
        !out0.presented_ids.contains(&w),
        "not presented ⇒ dormancy stops the forcing until the next paint"
    );
    // Once output 1's pieces are known, the same paint is output 1's.
    let out1 = build_with_elsewhere(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (800, 0, 800, 600),
        None,
        &nowhere,
    );
    assert_eq!(out1.stats.content_visible, 1);
    let elsewhere: std::collections::HashSet<_> = out1.pieces_ids.iter().copied().collect();
    let out0 = build_with_elsewhere(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
        &elsewhere,
    );
    assert_eq!(out0.stats.content_other_output, 1);
    assert!(!out0.stats.off_output_damage_forces_compose());
    assert!(!out0.snapshots.iter().any(|s| s.id == w));
}

/// The multi-output ack race, second half: output 0 is flip-pending when
/// the paint lands, so only output 1 walks; output 1 composes for its own
/// reasons, retires, and acks what it carried. W's damage must survive that
/// ack, and output 0's next walk must project it.
#[test]
fn an_output_never_acks_damage_it_did_not_present() {
    let (core, mut store, windows) = two_windows((700, 100, 200, 100), (0, 0, 10, 10));
    let w = drawable_of(&store, 0x100);
    // Output 0's most recent walk saw W (its retained pieces); no paint yet.
    let warm = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    let elsewhere: std::collections::HashSet<_> = warm.pieces_ids.iter().copied().collect();
    assert!(elsewhere.contains(&w));
    // The paint lands while output 0 is flip-pending: only output 1 walks.
    store.damage(w, rect(0, 0, 50, 100));
    let out1 = build_with_elsewhere(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (800, 0, 800, 600),
        None,
        &elsewhere,
    );
    // Output 1 composes (say, for its own cursor) and retires: it acks
    // exactly what it carried.
    for snap in out1.snapshots {
        store.ack_presentation_damage(snap);
    }
    assert!(
        !store.peek_presentation_damage(w).unwrap().region.is_empty(),
        "output 1 never presented W's damage, so its retire must not have acked it"
    );
    // Output 0 retires and walks: the highlight is still there to compose.
    let out0 = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    assert_eq!(projected_sorted(&out0), vec![rect(700, 100, 50, 100)]);
    assert!(
        out0.snapshots
            .iter()
            .any(|s| s.id == w && !s.region.is_empty())
    );
}

/// The multi-output ack race for a paint spanning both outputs: output 0
/// is flip-pending when it lands, output 1 composes it, and output 1's
/// retire acks it for everyone. The compose carries it in root
/// coordinates, and output 0 is handed its share as structure damage.
#[test]
fn a_spanning_paint_is_handed_to_the_output_that_did_not_compose_it() {
    let (core, mut store, windows) = two_windows((700, 100, 200, 100), (0, 0, 10, 10));
    let w = drawable_of(&store, 0x100);
    let warm = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    let out0_pieces: std::collections::HashSet<_> = warm.pieces_ids.iter().copied().collect();
    let out0_presented: std::collections::HashMap<_, _> =
        warm.snapshots.iter().map(|s| (s.id, s.epoch)).collect();
    store.damage(w, rect(0, 0, 200, 100));
    let out1 = build_with_elsewhere(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (800, 0, 800, 600),
        None,
        &out0_pieces,
    );
    let carried: Vec<_> = out1.carried.iter().filter(|c| c.id == w).collect();
    assert_eq!(carried.len(), 1);
    assert_eq!(carried[0].root, vec![rect(700, 100, 200, 100)]);
    let mut out0_damage = RegionSet::new();
    assert!(fan_out_to_output(
        (0, 0),
        extent(800, 600),
        &out0_pieces,
        &out0_presented,
        &mut out0_damage,
        &out1.carried,
    ));
    assert_eq!(out0_damage.rects(), &[rect(700, 100, 100, 100)]);
    for snap in out1.snapshots {
        store.ack_presentation_damage(snap);
    }
    let out0 = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    assert!(
        projected_sorted(&out0).is_empty(),
        "the store no longer holds it: the structure damage is all output 0 gets"
    );
}

/// An output that composed the drawable at that epoch or newer, or on
/// which it had no pieces, takes nothing.
#[test]
fn fan_out_skips_outputs_that_show_it_or_cannot() {
    let (_, store, _) = two_windows((700, 100, 200, 100), (0, 0, 10, 10));
    let w = drawable_of(&store, 0x100);
    let carried = [CarriedDamage {
        id: w,
        epoch: 5,
        root: vec![rect(700, 100, 200, 100)],
    }];
    let pieces: std::collections::HashSet<_> = [w].into_iter().collect();
    let mut damage = RegionSet::new();
    let at = |e: u64| -> std::collections::HashMap<_, _> { [(w, e)].into_iter().collect() };
    for presented in [at(5), at(6)] {
        assert!(!fan_out_to_output(
            (0, 0),
            extent(800, 600),
            &pieces,
            &presented,
            &mut damage,
            &carried,
        ));
    }
    assert!(!fan_out_to_output(
        (0, 0),
        extent(800, 600),
        &std::collections::HashSet::new(),
        &at(4),
        &mut damage,
        &carried,
    ));
    assert!(damage.is_empty());
    assert!(fan_out_to_output(
        (0, 0),
        extent(800, 600),
        &pieces,
        &at(4),
        &mut damage,
        &carried,
    ));
}

/// `Off` keeps the unclipped projection: what the legacy emitter damaged.
#[test]
fn off_mode_projection_is_unchanged_by_stage_c() {
    let (core, mut store, windows) = differential_fixture();
    // Paint into several windows, including ones the fixture covers.
    let xids: Vec<u32> = windows.keys().copied().collect();
    for (i, xid) in xids.iter().enumerate() {
        if let Some(id) = store.lookup(*xid) {
            #[allow(clippy::cast_possible_truncation)]
            let k = (i % 7) as i32;
            store.damage(id, rect(k, k, 8 + k as u32, 6));
        }
    }
    for layout in [(0, 0, 800, 600), (100, 50, 800, 600)] {
        let legacy = walk_with(true, &core, &mut store, &windows, layout, None);
        let off = walk_with(false, &core, &mut store, &windows, layout, None);
        assert!(
            !legacy.projected.is_empty(),
            "fixture damage must project at layout {layout:?}"
        );
        assert_eq!(off.projected, legacy.projected, "layout {layout:?}");
    }
    // And `On` projects a subset of `Off` (never more).
    let off = build_with(
        Visibility::Off,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    let on = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    let off_region = Region::from_rects(off.projected_damage.rects().iter().copied());
    for r in on.projected_damage.rects() {
        assert!(
            off_region.contains_rect(*r),
            "On projected {r:?} outside Off"
        );
    }
}

#[test]
fn only_off_output_damage_forces() {
    let mut s = WalkStats::default();
    assert!(!s.off_output_damage_forces_compose());
    s.content_hidden = 5;
    s.content_visible = 2;
    assert!(!s.off_output_damage_forces_compose());
    s.content_off_output = 1;
    assert!(s.off_output_damage_forces_compose());
}

#[test]
fn intersect_rects_clips_and_rejects_disjoint() {
    assert_eq!(
        intersect_rects(rect(0, 0, 10, 10), rect(5, 5, 10, 10)),
        Some(rect(5, 5, 5, 5))
    );
    assert_eq!(intersect_rects(rect(0, 0, 10, 10), rect(10, 0, 5, 5)), None);
    assert_eq!(
        intersect_rects(rect(0, 0, 10, 10), rect(-5, -5, 30, 30)),
        Some(rect(0, 0, 10, 10))
    );
}

// ── Step 1 stage B: walk cost bench ──────────────────────────────────

/// An e16-like tree on one 2560×1440 output: 10 unshaped top-levels, each
/// with 6 shaped leaf children of 8 rects, plus one large opaque window
/// covering half the screen, over a root.
fn e16_like_fixture() -> (
    KmsCore,
    DrawableStore,
    crate::kms::render::backend::WindowsMap,
) {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_root(&core, &mut store, 2560, 1440);
    let mut rank = 1u64;
    for t in 0..10i16 {
        let top = 0x1000 + u32::try_from(t).unwrap() * 0x10;
        let (tx, ty) = (40 + (t % 5) * 480, 60 + (t / 5) * 640);
        alloc_stub_window(&mut store, &mut windows, top, tx, ty, 440, 560, None, true);
        set_rank(&mut windows, top, rank);
        rank += 1;
        core.top_level_order.push(top);
        for c in 0..6i16 {
            let child = top + 1 + u32::try_from(c).unwrap();
            let (cx, cy) = (10 + (c % 3) * 140, 20 + (c / 3) * 260);
            alloc_stub_window(
                &mut store,
                &mut windows,
                child,
                cx,
                cy,
                128,
                240,
                Some(top),
                true,
            );
            set_rank(&mut windows, child, rank);
            rank += 1;
            // A frame-like shape: 4 edges + 4 corner nubs, all disjoint.
            core.shape_bounding.insert(
                child,
                vec![
                    xfixes::RegionRect {
                        x: 0,
                        y: 0,
                        width: 128,
                        height: 8,
                    },
                    xfixes::RegionRect {
                        x: 0,
                        y: 232,
                        width: 128,
                        height: 8,
                    },
                    xfixes::RegionRect {
                        x: 0,
                        y: 8,
                        width: 8,
                        height: 224,
                    },
                    xfixes::RegionRect {
                        x: 120,
                        y: 8,
                        width: 8,
                        height: 224,
                    },
                    xfixes::RegionRect {
                        x: 8,
                        y: 8,
                        width: 16,
                        height: 16,
                    },
                    xfixes::RegionRect {
                        x: 104,
                        y: 8,
                        width: 16,
                        height: 16,
                    },
                    xfixes::RegionRect {
                        x: 8,
                        y: 216,
                        width: 16,
                        height: 16,
                    },
                    xfixes::RegionRect {
                        x: 104,
                        y: 216,
                        width: 16,
                        height: 16,
                    },
                ],
            );
        }
    }
    // The big opaque window on top, covering the right half.
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x9000,
        1280,
        0,
        1280,
        1440,
        None,
        true,
    );
    set_rank(&mut windows, 0x9000, rank);
    core.top_level_order.push(0x9000);
    (core, store, windows)
}

/// `cargo test --release -p yserver -- --ignored walk_bench --nocapture`
#[test]
#[ignore = "bench: prints µs per walk, run in release with --nocapture"]
fn walk_bench() {
    let (core, mut store, windows) = e16_like_fixture();
    let layout = (0, 0, 2560u32, 1440u32);
    let platform = platform_with_layout(layout);
    let run = |mode: Visibility, store: &mut DrawableStore| {
        build_scene(
            &core, store, &windows, 0, &platform, None, None, None, false, mode,
        )
    };
    let warm = run(Visibility::On, &mut store);
    eprintln!("walk_bench: collapse split {:?}", warm.stats);
    eprintln!(
        "walk_bench: nodes={} draws={} hidden={} collapses={} (off draws={})",
        warm.stats.nodes_visited,
        warm.scene.draws.len(),
        warm.stats.hidden_participants,
        warm.stats.collapses(),
        run(Visibility::Off, &mut store).scene.draws.len(),
    );
    for mode in [Visibility::Off, Visibility::On] {
        let mut best = f64::MAX;
        for _ in 0..5 {
            let start = std::time::Instant::now();
            for _ in 0..1000 {
                let b = run(mode, &mut store);
                std::hint::black_box(&b);
            }
            let per = start.elapsed().as_secs_f64() * 1e6 / 1000.0;
            best = best.min(per);
        }
        eprintln!("walk_bench: {mode:?}: best of 5 runs = {best:.1} µs/walk");
    }
}

// ── #133 step 5 (P6) — the scene spaces and the coordinate recurrence ──
//
// Every test below has a named negative control: the one-line revert that
// makes it fail, verified by actually reverting. `bw == 0` identity is
// proved structurally by `a_borderless_node_needs_no_separate_child_region`
// and by the two pre-existing legacy-oracle tests
// (`refactored_emitter_matches_the_legacy_emitter_exactly`,
// `off_mode_reproduces_the_legacy_root_and_emitter_exactly`), which run a
// twelve-node `bw == 0` tree through the pre-#133 emitter and demand a
// byte-identical draw list.

/// A window whose storage is the BORDERED extent placed at the OUTER
/// origin, exactly as `allocate_window_leaf` builds it
/// (`backend.rs:13440`-`:13476`): extent `(w + 2bw) x (h + 2bw)` and the
/// allocation's `content_offset` recorded as `bw`.
#[allow(clippy::too_many_arguments)]
fn alloc_stub_window_bordered(
    store: &mut DrawableStore,
    windows: &mut crate::kms::render::backend::WindowsMap,
    xid: u32,
    x: i16,
    y: i16,
    w: u16,
    h: u16,
    bw: u16,
    parent: Option<u32>,
    mapped: bool,
) {
    let bordered = extent(
        u32::from(w) + 2 * u32::from(bw),
        u32::from(h) + 2 * u32::from(bw),
    );
    let mut storage =
        crate::kms::render::store::Storage::for_tests_null(bordered, vk::Format::B8G8R8A8_UNORM);
    let sentinel: ash::vk::ImageView = ash::vk::Handle::from_raw(u64::from(xid) | 0xFF00_0000);
    storage.image_view = sentinel;
    storage.sample_view = sentinel;
    let id = store
        .allocate(xid, DrawableKind::Window, 32, mapped, storage)
        .expect("bordered stub allocate");
    store.set_content_offset(id, i32::from(bw));
    windows.insert(
        xid,
        crate::kms::render::backend::WindowGeometry {
            border_width: bw,
            border_pixel: None,
            border_pixmap: None,
            x,
            y,
            width: w,
            height: h,
            depth: 32,
            mapped,
            viewable: true,
            parent,
            stack_rank: 0,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );
}

/// Sorted destination rects a given host xid contributed, under `mode`.
fn dst_rects_of(built: &SceneBuild, xid: u32) -> Vec<vk::Rect2D> {
    sorted_rects(draws_of(built, win_view(xid)))
}

fn r(x: i32, y: i32, w: u32, h: u32) -> vk::Rect2D {
    vk::Rect2D {
        offset: vk::Offset2D { x, y },
        extent: vk::Extent2D {
            width: w,
            height: h,
        },
    }
}

/// 5.1 — the node samples its OUTER extent from the storage ORIGIN. Before
/// step 5 the walk sampled `w x h` from `(0,0)` of a `(w+2bw) x (h+2bw)`
/// storage, so the bottom and right bands of the ring fell outside the
/// sampled rect entirely — the measured "only the top band" symptom.
///
/// Negative control: `let win_w = own_w;` (the pre-step-5 line) in
/// `decide_node` → the draw comes out 200x100 at (100,50) and every
/// assertion below fails.
#[test]
fn a_bordered_node_samples_its_whole_outer_extent() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_stub_window_bordered(
        &mut store,
        &mut windows,
        0x100,
        100,
        50,
        200,
        100,
        16,
        None,
        true,
    );
    core.top_level_order = vec![0x100];
    let off = build_with(
        Visibility::Off,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    // Outer origin = the window position; outer extent = w + 2bw.
    assert_eq!(dst_rects_of(&off, 0x100), vec![r(100, 50, 232, 132)]);
    // And the whole texture is sampled: src [0,0]-[1,1]. A wrong
    // denominator would show up here even when the dst rect is right.
    let d = off
        .scene
        .draws
        .iter()
        .find(|d| ash::vk::Handle::as_raw(d.image_view) == win_view(0x100))
        .expect("bordered draw");
    assert_eq!(d.src_origin, [0.0, 0.0]);
    assert_eq!(d.src_size, [1.0, 1.0]);
}

/// 5.1 — the two-absolute recurrence. This is the awesome bug in
/// miniature: the child's parent-relative position is measured from the
/// parent's CONTENT origin (`parent_outer + bw`), not from its outer one.
///
/// Negative control: pass `abs_x, abs_y` instead of
/// `node.content_abs_x, node.content_abs_y` at the recursion → the child
/// lands at (100, 67) instead of (116, 83), i.e. displaced by `bw` in
/// both axes, and it overpaints the parent's left bar just as the scanout
/// showed.
#[test]
fn a_bordered_parents_child_is_placed_from_the_content_origin() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_stub_window_bordered(
        &mut store,
        &mut windows,
        0x100,
        100,
        50,
        200,
        100,
        16,
        None,
        true,
    );
    // Awesome's frame-relative titlebar child: (0, 17) inside the frame.
    alloc_stub_window_bordered(
        &mut store,
        &mut windows,
        0x101,
        0,
        17,
        100,
        20,
        0,
        Some(0x100),
        true,
    );
    set_rank(&mut windows, 0x100, 1);
    set_rank(&mut windows, 0x101, 2);
    core.top_level_order = vec![0x100];
    let off = build_with(
        Visibility::Off,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    // content origin = (100 + 16, 50 + 16) = (116, 66); child at +(0, 17).
    assert_eq!(dst_rects_of(&off, 0x101), vec![r(116, 83, 100, 20)]);
}

/// 5.2 — a child may not overlap its parent's border
/// (`mi/mivaltree.c:386`). An oversized child is clipped to the parent's
/// INNER region, so the ring survives on all four sides even though the
/// child is larger than the whole outer rect.
///
/// Negative control: `let child_clip_x0 = clip_x0.max(abs_x);` (and the
/// three siblings) in `decide_node`, i.e. clip children to the OUTER rect
/// → the child's rect becomes (0,0,132,132) and it covers the ring.
#[test]
fn a_child_cannot_overlap_its_parents_border() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_root(&core, &mut store, 800, 600);
    alloc_stub_window_bordered(
        &mut store,
        &mut windows,
        0x100,
        0,
        0,
        100,
        100,
        16,
        None,
        true,
    );
    alloc_stub_window_bordered(
        &mut store,
        &mut windows,
        0x101,
        0,
        0,
        300,
        300,
        0,
        Some(0x100),
        true,
    );
    set_rank(&mut windows, 0x100, 1);
    set_rank(&mut windows, 0x101, 2);
    core.top_level_order = vec![0x100];
    let on = assert_oracle(
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
        "child vs parent border",
    );
    // The child is confined to the parent's content box.
    assert_eq!(dst_rects_of(&on, 0x101), vec![r(16, 16, 100, 100)]);
    // The parent keeps exactly the annulus: 132² − 100² = 7 424 px, split
    // into the four bands the visibility walk left it.
    let parent: u64 = dst_rects_of(&on, 0x100).iter().map(|x| area_of(*x)).sum();
    assert_eq!(parent, 132 * 132 - 100 * 100);
    // All four sides present, not just the top band.
    let rects = dst_rects_of(&on, 0x100);
    let covers = |px: i32, py: i32| {
        rects.iter().any(|q| {
            px >= q.offset.x
                && py >= q.offset.y
                && px < q.offset.x + i32::try_from(q.extent.width).unwrap()
                && py < q.offset.y + i32::try_from(q.extent.height).unwrap()
        })
    };
    assert!(covers(66, 8), "top band");
    assert!(covers(66, 123), "bottom band");
    assert!(covers(8, 66), "left bar");
    assert!(covers(123, 66), "right bar");
}

/// 5.3 — SHAPE regions are WINDOW-LOCAL (content-relative), so a bordered
/// shaped window's mask must be read at `+ bw` in storage space
/// (`dix/window.c:1736`). And `SetBorderSize` intersects the expanded box
/// with the bounding shape (`:1772`), so a bounding shape that stops at
/// the content edge removes the ring — the bounding region is the
/// border-INCLUSIVE extent.
///
/// Negative control: `let rx = i32::from(rect.x);` in the `place` loop →
/// the mask lands at (10, 10) instead of (18, 18), off by `bw`.
#[test]
fn a_bordered_shaped_windows_mask_is_read_in_content_coordinates() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_stub_window_bordered(
        &mut store,
        &mut windows,
        0x100,
        10,
        10,
        100,
        100,
        8,
        None,
        true,
    );
    core.top_level_order = vec![0x100];
    // One bounding rect covering the top-left quarter of the CONTENT.
    core.shape_bounding.insert(
        0x100,
        vec![xfixes::RegionRect {
            x: 0,
            y: 0,
            width: 50,
            height: 50,
        }],
    );
    let off = build_with(
        Visibility::Off,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    // Window at (10,10) with bw=8 ⇒ content origin (18,18); the mask is
    // relative to THAT, and the ring outside it is gone.
    assert_eq!(dst_rects_of(&off, 0x100), vec![r(18, 18, 50, 50)]);
}

/// 5.2 / 5.3 — the CLIP shape narrows `winSize` (what descendants clip to)
/// and nothing else: `SetWinSize` intersects with both shapes
/// (`dix/window.c:1735`), `SetBorderSize` with the bounding shape only
/// (`:1772`). Bounding, clip and input shapes are three different things.
///
/// Negative control: drop `clip` from the `[bounding, clip]` array in
/// `inner_place_rects` → the child keeps its full 100x100 rect.
#[test]
fn a_clip_shape_narrows_the_child_clip_but_not_the_nodes_own_place() {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_root(&core, &mut store, 800, 600);
    alloc_stub_window(&mut store, &mut windows, 0x100, 0, 0, 100, 100, None, true);
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x101,
        0,
        0,
        100,
        100,
        Some(0x100),
        true,
    );
    set_rank(&mut windows, 0x100, 1);
    set_rank(&mut windows, 0x101, 2);
    core.top_level_order = vec![0x100];
    core.shape_clip.insert(
        0x100,
        vec![xfixes::RegionRect {
            x: 0,
            y: 0,
            width: 40,
            height: 100,
        }],
    );
    let on = build_with(
        Visibility::On,
        &core,
        &mut store,
        &windows,
        (0, 0, 800, 600),
        None,
    );
    // The clip shape does not touch the parent's own region…
    assert_eq!(
        dst_rects_of(&on, 0x100)
            .iter()
            .map(|x| area_of(*x))
            .sum::<u64>(),
        100 * 100 - 40 * 100,
        "parent draws its whole rect; the child claims only the clipped part",
    );
    // …but the child is confined to it.
    assert_eq!(dst_rects_of(&on, 0x101), vec![r(0, 0, 40, 100)]);
}

/// 5.4 — the redirection exception is a RESET, not an intersect
/// (`mi/mivaltree.c:233`-`:239`). A window that owns its own
/// `redirected_target` is NOT clipped to its parent
/// (`dix/window.c:1719`, `:1753`), and the reset propagates: it happens
/// before the `∩ winSize` at `:390`, so its descendants inherit the
/// un-parent-clipped region too.
///
/// Negative control: delete the `if self_owns_redirected_target` reset in
/// `decide_node` → the redirected child is clipped to its parent's
/// content box (40x50 instead of 100x50) and its grandchild disappears
/// entirely.
#[test]
fn a_redirected_window_is_not_clipped_to_its_parent_and_neither_are_its_children() {
    for participating in [true, false] {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let mut windows = crate::kms::render::backend::WindowsMap::new();
        // Parent 100x100 at (0,0); the redirected child starts at (60,0)
        // and is 100 wide, so 60 px of it stick out past the parent.
        alloc_stub_window(&mut store, &mut windows, 0x100, 0, 0, 100, 100, None, true);
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x101,
            60,
            0,
            100,
            50,
            Some(0x100),
            true,
        );
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x102,
            0,
            0,
            100,
            50,
            Some(0x101),
            true,
        );
        set_rank(&mut windows, 0x100, 1);
        set_rank(&mut windows, 0x101, 2);
        set_rank(&mut windows, 0x102, 3);
        core.top_level_order = vec![0x100];
        let c_id = store.lookup(0x101).expect("redirected child present");
        let backing = alloc_backing(&mut store, 0xB101, 100, 50);
        store.set_redirected_target(c_id, Some(backing));
        // Automatic (`true`) and manual (`false`) alike: Xorg keys the
        // reset off `redirectDraw != RedirectDrawNone`.
        store.set_scene_participating(c_id, participating);
        // The grandchild owns its own backing so that it emits — a plain
        // descendant's paint lands in the redirected ancestor's backing
        // and is skipped by design, which would make the placement
        // unobservable.
        let g_id = store.lookup(0x102).expect("grandchild present");
        let g_backing = alloc_backing(&mut store, 0xB102, 100, 50);
        store.set_redirected_target(g_id, Some(g_backing));
        let off = build_with(
            Visibility::Off,
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
        );
        // The redirected window's own draw (automatic mode only — a
        // manual one is skipped from the scene by design) keeps its full
        // width, not the 40 px its parent would have allowed.
        let backing_rects = sorted_rects(draws_of(&off, u64::from(0xB101u32) | 0xB000_0000));
        if participating {
            assert_eq!(backing_rects, vec![r(60, 0, 100, 50)], "automatic");
        } else {
            assert!(backing_rects.is_empty(), "manual is skipped from the scene");
        }
        // Rule 3: the reset PROPAGATES. The grandchild inherits the
        // un-parent-clipped region and keeps its full 100 px width, in
        // both redirect modes.
        assert_eq!(
            sorted_rects(draws_of(&off, u64::from(0xB102u32) | 0xB000_0000)),
            vec![r(60, 0, 100, 50)],
            "grandchild inherits the reset, participating={participating}",
        );
    }
}

/// 5.1 / 5.4 — a bordered child under a redirected ancestor is not
/// displaced, in either redirect mode. The child owns its own backing so
/// that it emits and the placement is observable.
///
/// Negative control: revert the recursion to `abs_x, abs_y` → the child
/// lands at (210, 120) − nothing, but the GRANDCHILD lands at (218, 128)
/// instead of (226, 136), off by the child's own `bw`.
#[test]
fn a_bordered_child_under_a_redirected_ancestor_is_not_displaced() {
    for participating in [true, false] {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let mut windows = crate::kms::render::backend::WindowsMap::new();
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x100,
            200,
            100,
            400,
            300,
            None,
            true,
        );
        alloc_stub_window_bordered(
            &mut store,
            &mut windows,
            0x101,
            10,
            20,
            100,
            50,
            8,
            Some(0x100),
            true,
        );
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x102,
            0,
            0,
            20,
            10,
            Some(0x101),
            true,
        );
        set_rank(&mut windows, 0x100, 1);
        set_rank(&mut windows, 0x101, 2);
        set_rank(&mut windows, 0x102, 3);
        core.top_level_order = vec![0x100];
        // The top-level is redirected; the bordered child owns its own
        // backing so it still emits (`has_own_redirected_target` breaks
        // the ancestor chain).
        let top_id = store.lookup(0x100).expect("top present");
        let top_backing = alloc_backing(&mut store, 0xB100, 400, 300);
        store.set_redirected_target(top_id, Some(top_backing));
        store.set_scene_participating(top_id, participating);
        let child_id = store.lookup(0x101).expect("child present");
        // The backing of a bordered window is the BORDERED extent placed
        // at the outer origin (`compAllocPixmap`, `compalloc.c:610`), and
        // it carries the same content offset.
        let child_backing = alloc_backing(&mut store, 0xB101, 116, 66);
        store.set_redirected_target(child_id, Some(child_backing));
        store.set_content_offset(child_backing, 8);
        // The grandchild needs its own backing too: a plain descendant of
        // a redirected window paints into the ancestor's backing and is
        // skipped from the scene, which would make its placement
        // unobservable.
        let g_id = store.lookup(0x102).expect("grandchild present");
        let g_backing = alloc_backing(&mut store, 0xB102, 20, 10);
        store.set_redirected_target(g_id, Some(g_backing));
        let off = build_with(
            Visibility::Off,
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
        );
        // Outer = top content (200,100, no border) + (10,20) = (210,120),
        // extent 100 + 2·8 = 116 by 66.
        assert_eq!(
            sorted_rects(draws_of(&off, u64::from(0xB101u32) | 0xB000_0000)),
            vec![r(210, 120, 116, 66)],
            "participating={participating}",
        );
        // The grandchild is positioned from the bordered child's CONTENT
        // origin: (210 + 8, 120 + 8) = (218, 128).
        assert_eq!(
            sorted_rects(draws_of(&off, u64::from(0xB102u32) | 0xB000_0000)),
            vec![r(218, 128, 20, 10)],
            "participating={participating}",
        );
    }
}

/// `bw == 0` identity, structurally: a borderless window with no clip
/// shape produces NO separate child region — `child_place` is `None`, so
/// `mine` and the non-opaque claim read the very same `place` vector the
/// pre-step-5 walk read, through the same code. The moment either a
/// border or a clip shape exists, the two regions separate.
///
/// Negative control: make `child_place` unconditionally `Some(...)` →
/// the first assertion fails (and the two legacy-oracle differential
/// tests still pass, which is why this structural check is needed on top
/// of them).
#[test]
fn a_borderless_node_needs_no_separate_child_region() {
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    let empty: HashMap<u32, Vec<xfixes::RegionRect>> = HashMap::new();
    let mut clip: HashMap<u32, Vec<xfixes::RegionRect>> = HashMap::new();
    alloc_stub_window(&mut store, &mut windows, 0x100, 10, 20, 100, 50, None, true);
    alloc_stub_window_bordered(
        &mut store,
        &mut windows,
        0x200,
        10,
        20,
        100,
        50,
        4,
        None,
        true,
    );
    let decide = |xid: u32,
                  store: &DrawableStore,
                  windows: &crate::kms::render::backend::WindowsMap,
                  shape_clip: &HashMap<u32, Vec<xfixes::RegionRect>>| {
        decide_node(
            xid,
            windows.get(&xid).expect("geom"),
            0,
            0,
            store,
            &empty,
            shape_clip,
            0,
            0,
            800,
            600,
            Visibility::On,
            false,
            false,
            i32::MIN / 2,
            i32::MIN / 2,
            i32::MAX / 2,
            i32::MAX / 2,
        )
    };
    let plain = decide(0x100, &store, &windows, &empty);
    assert!(
        plain.child_place.is_none(),
        "bw == 0, no clip shape ⇒ one region serves both roles",
    );
    assert_eq!((plain.abs_x, plain.abs_y), (10, 20));
    assert_eq!(
        (plain.content_abs_x, plain.content_abs_y),
        (10, 20),
        "outer and content coincide at bw == 0",
    );
    assert_eq!((plain.win_w, plain.win_h), (100, 50));

    let bordered = decide(0x200, &store, &windows, &empty);
    assert_eq!(
        bordered.child_place,
        Some(vec![r(14, 24, 100, 50)]),
        "bw > 0 ⇒ the inner region is the content box",
    );
    assert_eq!(bordered.place, vec![r(10, 20, 108, 58)]);
    assert_eq!((bordered.content_abs_x, bordered.content_abs_y), (14, 24));

    // A clip shape alone also separates the two regions, at bw == 0.
    clip.insert(
        0x100,
        vec![xfixes::RegionRect {
            x: 0,
            y: 0,
            width: 40,
            height: 50,
        }],
    );
    let clipped = decide(0x100, &store, &windows, &clip);
    assert_eq!(
        clipped.place,
        vec![r(10, 20, 100, 50)],
        "place is untouched"
    );
    assert_eq!(clipped.child_place, Some(vec![r(10, 20, 40, 50)]));
}

/// The measured awesome regression, reproduced from the scanout dump.
///
/// Two tiles on a 2560x1440 output: outer 1276x1421 at (0, 17) and
/// (1276, 17), `bw = 16`, content 1244x1389, each with awesome's
/// frame-relative (0, 17) child sized 1244x1372 (the geometries in the
/// `yserver-drawable-0-win-*` dumps). Before step 5 the scan found 19 920
/// red plus 19 920 green pixels — 16 rows of 1244 plus a single row of 16
/// — all in the top band. After it, each tile's ring is
/// `2·bw·(outer_w + outer_h − 2·bw) = 32 · (1276 + 1421 − 32) = 85 280`
/// pixels, on all four sides, and the 32 px inter-tile gap at x 1244..1275
/// is two adjacent border bars rather than wallpaper.
///
/// Negative control: any one of the three reverts named above — the outer
/// sampling extent, the content-origin recursion, or the inner child clip
/// — breaks a different assertion here.
#[test]
fn the_awesome_two_tile_layout_rings_all_four_sides() {
    const BW: u16 = 16;
    const CW: u16 = 1244;
    const CH: u16 = 1389;
    const OW: u32 = CW as u32 + 2 * BW as u32; // 1276
    const OH: u32 = CH as u32 + 2 * BW as u32; // 1421
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_root(&core, &mut store, 5120, 1440);
    for (i, (xid, child, x)) in [(0x100u32, 0x101u32, 0i16), (0x200, 0x201, 1276)]
        .into_iter()
        .enumerate()
    {
        alloc_stub_window_bordered(&mut store, &mut windows, xid, x, 17, CW, CH, BW, None, true);
        alloc_stub_window_bordered(
            &mut store,
            &mut windows,
            child,
            0,
            17,
            CW,
            1372,
            0,
            Some(xid),
            true,
        );
        set_rank(&mut windows, xid, (2 * i + 1) as u64);
        set_rank(&mut windows, child, (2 * i + 2) as u64);
    }
    core.top_level_order = vec![0x100, 0x200];
    let on = assert_oracle(
        &core,
        &mut store,
        &windows,
        (0, 0, 2560, 1440),
        None,
        "awesome two tiles",
    );

    for (xid, child, x0) in [(0x100u32, 0x101u32, 0i32), (0x200, 0x201, 1276)] {
        // The frame's visible region is exactly the ring: the child fills
        // the content from y = 33 down and the rest of the content is
        // still the frame's own.
        let rects = dst_rects_of(&on, xid);
        let covers = |px: i32, py: i32| {
            rects.iter().any(|q| {
                px >= q.offset.x
                    && py >= q.offset.y
                    && px < q.offset.x + i32::try_from(q.extent.width).unwrap()
                    && py < q.offset.y + i32::try_from(q.extent.height).unwrap()
            })
        };
        // All four bands and all four corners.
        assert!(covers(x0 + 638, 24), "{xid:#x} top band");
        assert!(covers(x0 + 638, 1430), "{xid:#x} bottom band");
        assert!(covers(x0 + 8, 700), "{xid:#x} left bar");
        assert!(covers(x0 + 1268, 700), "{xid:#x} right bar");
        for (cx, cy) in [(0, 17), (1275, 17), (0, 1437), (1275, 1437)] {
            assert!(covers(x0 + cx, cy), "{xid:#x} corner ({cx},{cy})");
        }
        // The ring's area, plus the strip of content above the child.
        let area: u64 = rects.iter().map(|q| area_of(*q)).sum();
        let ring = 2 * u64::from(BW) * (u64::from(OW) + u64::from(OH) - 2 * u64::from(BW));
        assert_eq!(ring, 85_280, "the predicted per-tile ring area");
        assert_eq!(
            area,
            ring + u64::from(CW) * 17,
            "{xid:#x}: ring + the 17 rows of content above the titlebar child",
        );
        // Content lands at x0 + 16 with the titlebar at y = 33.
        assert_eq!(dst_rects_of(&on, child), vec![r(x0 + 16, 50, 1244, 1372)]);
    }
    // The 32 px inter-tile gap between the two CONTENTS — x 1260..1291,
    // i.e. tile 1's right bar (1260..1275) plus tile 2's left bar
    // (1276..1291) — is border, not wallpaper. Before step 5 the scan
    // found wallpaper in a 32 px column here.
    assert!(
        dst_rects_of(&on, 0x100)
            .iter()
            .any(|q| q.offset.x == 1260 && q.extent.width == u32::from(BW)),
        "tile 1's right bar is 16 px wide at x = 1260",
    );
    assert!(
        dst_rects_of(&on, 0x200)
            .iter()
            .any(|q| q.offset.x == 1276 && q.extent.width == u32::from(BW)),
        "tile 2's left bar is 16 px wide at x = 1276",
    );
    // And nothing of the wallpaper survives inside that column.
    let gap = r(1260, 17, 32, 1421);
    for q in sorted_rects(draws_of(&on, 0x00A0_7000)) {
        assert!(
            intersect_rects(q, gap).is_none(),
            "wallpaper {q:?} shows through the inter-tile gap {gap:?}",
        );
    }
}

/// #133 step 5 (5.2) — the child-clip bound is ABSOLUTE, not
/// storage-local, for a bordered parent at a non-zero origin.
///
/// The exact numbers from the reporter's matched dump + scanout
/// (bee, `border_width = 32`): frame `geom=(0,17 1136x1086)`,
/// `content_offset = 32`, storage 1200x1150. So
///
/// ```text
/// abs_y          = 0 + 17          =   17   (outer, screen space)
/// content_abs_y  = 17 + 32         =   49   (content, screen space)
/// child_clip_y1  = 49 + 1086       = 1135   (screen space)
/// ```
///
/// The value that would produce the measured 17-row band is **1118**,
/// which is `co + own_h = 32 + 1086` — the end of the content in the
/// parent's own STORAGE space, where the frame's ring begins
/// (`ring green y 1118..1149` in that dump). A storage-space bound
/// leaking into an absolute clip would shorten a TIGHT-FIT child by
/// exactly the parent's outer `y`. This pins which of the two the
/// code computes.
///
/// Negative control: `clip_y1.min(co + own_h)` in `decide_node` →
/// `child_clip_y1` comes out 1118 and this test fails.
#[test]
fn the_child_clip_bound_is_absolute_not_storage_local() {
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    let empty: HashMap<u32, Vec<xfixes::RegionRect>> = HashMap::new();
    // awesome's frame, at the reporter's exact geometry.
    alloc_stub_window_bordered(
        &mut store,
        &mut windows,
        0x100,
        0,
        17,
        1136,
        1086,
        32,
        None,
        true,
    );
    let node = decide_node(
        0x100,
        windows.get(&0x100).expect("geom"),
        0,
        0,
        &store,
        &empty,
        &empty,
        0,
        0,
        2560,
        1440,
        Visibility::On,
        false,
        false,
        i32::MIN / 2,
        i32::MIN / 2,
        i32::MAX / 2,
        i32::MAX / 2,
    );
    assert_eq!((node.abs_x, node.abs_y), (0, 17), "outer absolute");
    assert_eq!(
        (node.content_abs_x, node.content_abs_y),
        (32, 49),
        "content absolute = outer + content_offset",
    );
    assert_eq!(
        (node.child_clip_y0, node.child_clip_y1),
        (49, 1135),
        "child clip is ABSOLUTE: 49..49+1086, NOT the storage-space 32..1118",
    );
    assert_eq!(
        (node.child_clip_x0, node.child_clip_x1),
        (32, 1168),
        "same in x: 32..32+1136",
    );
    // And the parent's own outer placement, for contrast: screen
    // (0,17) with the bordered extent.
    assert_eq!(node.place, vec![r(0, 17, 1200, 1150)]);
    // The inner region descendants are clipped to, in output space:
    // the content box, NOT the storage box.
    assert_eq!(node.child_place, Some(vec![r(32, 49, 1136, 1086)]));
}

/// Reallocate a bordered stub at a new size, as
/// `sync_window_leaf_storage` does on a resize: detach the xid, then
/// allocate fresh storage at the new bordered extent. The fresh
/// allocation gets a NEW `DrawableId`, which is what makes the scene
/// diff see the participant as replaced rather than moved.
#[allow(clippy::too_many_arguments)]
fn resize_stub_window_bordered(
    store: &mut DrawableStore,
    windows: &mut crate::kms::render::backend::WindowsMap,
    xid: u32,
    w: u16,
    h: u16,
    bw: u16,
) {
    store.detach_xid(xid);
    let bordered = extent(
        u32::from(w) + 2 * u32::from(bw),
        u32::from(h) + 2 * u32::from(bw),
    );
    let mut storage =
        crate::kms::render::store::Storage::for_tests_null(bordered, vk::Format::B8G8R8A8_UNORM);
    let sentinel: ash::vk::ImageView = ash::vk::Handle::from_raw(u64::from(xid) | 0xFF00_0000);
    storage.image_view = sentinel;
    storage.sample_view = sentinel;
    let id = store
        .allocate(xid, DrawableKind::Window, 32, true, storage)
        .expect("resize stub allocate");
    store.set_content_offset(id, i32::from(bw));
    let g = windows.get_mut(&xid).expect("resize stub geom");
    g.width = w;
    g.height = h;
    g.border_width = bw;
}

/// #133 — a GROW must damage the window's full new outer rect.
///
/// The walk is exonerated for the wezterm white band: with the
/// reporter's matched 20:14 dump the titlebar sits at 49..65, the
/// client is placed at 66, `abs_y = 17` / `content_abs_y = 49` are
/// both present and `own_h = 1086`, so `child_clip_y1 = 1135` and
/// `vis_ly1 = 1069` — the walk emits the client's full 1069 rows
/// (pinned by `the_child_clip_bound_is_absolute_not_storage_local`
/// and the `ProtoFixture` tight-fit test). Compose is
/// damage-clipped, so the remaining way for a correct draw list not
/// to reach the screen is for the newly-exposed rows never to be
/// repainted.
///
/// `structural_damage` is what reports a resize once the coarse
/// `mark_scene_structure_dirty` hammer is demoted, so this asserts
/// it directly rather than through pixels: the diff of the
/// pre-grow and post-grow participant lists must CONTAIN every new
/// outer rect. The awesome shape, tight-fit, frame at a non-zero
/// origin, all three levels reallocated as the real resize path
/// does (fresh `DrawableId` per realloc).
///
/// Negative control: in `structural_damage`, drop
/// `damage.union_with(&p.region)` from the `None => ...` arm (the
/// appeared-participant case) → the new extents are not damaged and
/// every `contains` below fails.
#[test]
fn a_grow_damages_the_full_new_outer_rect() {
    const BW: u16 = 16;
    const TITLE: i16 = 17;
    const FX: i16 = 0;
    const FY: i16 = 17;
    let small = (200u16, 120u16);
    let big = (400u16, 260u16);

    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_root(&core, &mut store, 800, 600);
    // frame content = client height + titlebar (awesome's rule), so the
    // client exactly fills the remaining content: the tight fit.
    alloc_stub_window_bordered(
        &mut store,
        &mut windows,
        0x100,
        FX,
        FY,
        small.0,
        small.1 + TITLE as u16,
        BW,
        None,
        true,
    );
    alloc_stub_window_bordered(
        &mut store,
        &mut windows,
        0x101,
        0,
        TITLE,
        small.0,
        small.1,
        0,
        Some(0x100),
        true,
    );
    alloc_stub_window_bordered(
        &mut store,
        &mut windows,
        0x102,
        0,
        0,
        small.0,
        small.1,
        0,
        Some(0x101),
        true,
    );
    set_rank(&mut windows, 0x100, 1);
    set_rank(&mut windows, 0x101, 2);
    set_rank(&mut windows, 0x102, 3);
    core.top_level_order = vec![0x100];

    let layout = (0, 0, 800u32, 600u32);
    let before = build_with(Visibility::On, &core, &mut store, &windows, layout, None);

    // The grow, in the trace's order: frame, client, then the GL child.
    resize_stub_window_bordered(
        &mut store,
        &mut windows,
        0x100,
        big.0,
        big.1 + TITLE as u16,
        BW,
    );
    resize_stub_window_bordered(&mut store, &mut windows, 0x101, big.0, big.1, 0);
    resize_stub_window_bordered(&mut store, &mut windows, 0x102, big.0, big.1, 0);

    let after = build_with(Visibility::On, &core, &mut store, &windows, layout, None);
    let damage = structural_damage(&before.participants, &after.participants);

    // Sanity: the fixture really grew, and really is a tight fit.
    assert_eq!(
        i32::from(TITLE) + i32::from(big.1),
        i32::from(big.1 + TITLE as u16),
        "tight fit",
    );
    let frame_outer = r(
        i32::from(FX),
        i32::from(FY),
        u32::from(big.0) + 2 * u32::from(BW),
        u32::from(big.1 + TITLE as u16) + 2 * u32::from(BW),
    );
    let content_origin = (i32::from(FX) + i32::from(BW), i32::from(FY) + i32::from(BW));
    let client_rect = r(
        content_origin.0,
        content_origin.1 + i32::from(TITLE),
        u32::from(big.0),
        u32::from(big.1),
    );
    assert_eq!(
        sorted_rects(draws_of(&after, win_view(0x102))),
        vec![client_rect],
        "fixture sanity: the grown GL child is placed and sized correctly",
    );

    // THE ASSERTION: the damage a grow reports must cover every new
    // extent. A tail that is emitted but not damaged is a tail the
    // damage-clipped compose never writes, and it keeps whatever the
    // scanout BO held — which is `ffffff00` uninitialised storage,
    // i.e. opaque white.
    for (name, rect) in [
        ("frame new outer rect", frame_outer),
        ("client new rect", client_rect),
    ] {
        assert!(
            damage.contains_rect(rect),
            "{name} {rect:?} is not covered by a grow's structural damage: {:?}",
            sorted_rects(damage.rects().collect()),
        );
    }
    // And specifically the newly-exposed TAIL — the rows the grow
    // uncovered at the bottom, which is where the reported band is.
    let tail = r(
        client_rect.offset.x,
        i32::from(TITLE) + content_origin.1 + i32::from(small.1),
        u32::from(big.0),
        u32::from(big.1 - small.1),
    );
    assert!(
        damage.contains_rect(tail),
        "the newly-exposed tail {tail:?} is not damaged: {:?}",
        sorted_rects(damage.rects().collect()),
    );
}

/// #133 — the bee tree at its real numbers, with awesome's wibar as a
/// higher top-level sibling that claims from the universe first.
///
/// `Visibility::On` must produce the same PIXELS as `Off`, and the
/// draw list must cover the whole client rect. The measured mismatch
/// on bee is at `pixel=32,1118` with `candidate=0xffffffff` /
/// `reference=0xff000000`, damage covering the entire output, zero
/// clipped repaints, zero collapses and `hidden_participants/s=0` —
/// the last of which is a direct contradiction with the fixture, where
/// the GL child covers its client exactly and the client is therefore
/// hidden. `draws=8` for five participants means the client emits ONE
/// draw, i.e. the GL child does not cover it.
#[test]
fn the_bee_tree_with_a_higher_wibar_sibling_matches_the_unclipped_scene() {
    const BW: u16 = 32;
    const TITLE: i16 = 17;
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_root(&core, &mut store, 2560, 1440);
    // awesome's wibar: full width, 17 tall, at the very top.
    alloc_stub_window_bordered(
        &mut store,
        &mut windows,
        0x010,
        0,
        0,
        2560,
        17,
        0,
        None,
        true,
    );
    // The frame, at the reporter's exact geometry.
    alloc_stub_window_bordered(
        &mut store,
        &mut windows,
        0x100,
        0,
        17,
        1136,
        1086,
        BW,
        None,
        true,
    );
    alloc_stub_window_bordered(
        &mut store,
        &mut windows,
        0x101,
        0,
        TITLE,
        1136,
        1069,
        0,
        Some(0x100),
        true,
    );
    alloc_stub_window_bordered(
        &mut store,
        &mut windows,
        0x102,
        0,
        0,
        1136,
        1069,
        0,
        Some(0x101),
        true,
    );
    set_rank(&mut windows, 0x100, 1);
    set_rank(&mut windows, 0x101, 2);
    set_rank(&mut windows, 0x102, 3);
    // The wibar is stacked ABOVE the frame, so it claims first.
    core.top_level_order = vec![0x100, 0x010];

    let layout = (0, 0, 2560u32, 1440u32);
    let on = build_with(Visibility::On, &core, &mut store, &windows, layout, None);
    let off = build_with(Visibility::Off, &core, &mut store, &windows, layout, None);
    eprintln!(
        "ON  draws={} hidden={} collapses={:?}",
        on.scene.draws.len(),
        on.stats.hidden_participants,
        (
            on.stats.collapses_mine,
            on.stats.collapses_claim,
            on.stats.collapses_taken,
            on.stats.collapses_taken_skipped
        ),
    );
    for (name, xid) in [
        ("wibar", 0x010u32),
        ("frame", 0x100),
        ("client", 0x101),
        ("glchild", 0x102),
    ] {
        eprintln!(
            "  {name}: ON {:?}\n           OFF {:?}",
            sorted_rects(draws_of(&on, win_view(xid))),
            sorted_rects(draws_of(&off, win_view(xid))),
        );
    }
    // The GL child must cover the whole client rect: content origin
    // (32, 49) + (0, 17) = (32, 66), 1136x1069 → y 66..1135.
    assert_eq!(
        sorted_rects(draws_of(&on, win_view(0x102))),
        vec![r(32, 66, 1136, 1069)],
        "the GL child's ON draw must be its full unbroken rect",
    );
    // Nothing may be left for the client to emit — the reported
    // `draws=8` / `hidden_participants=0` says otherwise on bee.
    assert!(
        draws_of(&on, win_view(0x101)).is_empty(),
        "the client is fully covered by its GL child, so it emits nothing: {:?}",
        sorted_rects(draws_of(&on, win_view(0x101))),
    );
    assert_eq!(on.stats.hidden_participants, 1, "the client is hidden");
    // And the pixel oracle over the whole output.
    let _ = assert_oracle(
        &core,
        &mut store,
        &windows,
        layout,
        None,
        "bee tree + wibar",
    );
}
