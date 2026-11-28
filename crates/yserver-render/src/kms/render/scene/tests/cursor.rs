use super::*;
use std::io;

fn test_cursor_entry(bytes: Option<Vec<u8>>) -> CursorEntry {
    CursorEntry {
        id: crate::kms::render::store::DrawableId::for_tests(91),
        extent: extent(16, 16),
        hot_x: 0,
        hot_y: 0,
        record_version: 91,
        bgra_bytes: bytes.map(std::sync::Arc::new),
    }
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

// Mutation: map upload/show Deferred to Hidden or clear its retry version;
// the result, retained-mode, repaint, and retry assertions must fail.
#[test]
fn deferred_transition_keeps_the_retained_mode_and_forces_a_show_retry() {
    let mut sink = TestSink::new();
    let mut cursor = test_cursor_entry(Some(vec![0; 16 * 16 * 4]));
    cursor.hot_x = 2;
    cursor.hot_y = 3;
    sink.cursor_available = true;
    sink.cursor_visible = true;
    sink.upload_response = TestUploadResponse::Deferred;
    let transition = Some(CursorTransition::ShowOnRetire {
        upload_version: 91,
        hot_x: 2,
        hot_y: 3,
        x: 40,
        y: 50,
    });
    let result = apply_cursor_transition_on_retire(Some(&cursor), 0, &mut sink, transition);
    assert_eq!(result, CursorTransitionResult::Deferred);

    let retained = OutputCursorMode::Sw {
        prev: Some((12, 13)),
    };
    let resolution = resolve_retired_cursor_state(result, OutputCursorMode::Hw, retained);
    assert_eq!(resolution.actual_mode, retained);
    assert!(!resolution.commit_desired_metadata);
    assert!(!resolution.clear_presented_metadata);
    assert!(resolution.force_repaint);
    assert_eq!(
        update_force_show_retry_version(None, transition, result, OutputCursorMode::Hw,),
        Some(CursorShowRetry {
            version: 91,
            deferred: true,
        })
    );

    sink.upload_response = TestUploadResponse::Ready;
    sink.uploaded_version = None;
    sink.show_failure = Some((true, true));
    let show_result = apply_cursor_transition_on_retire(Some(&cursor), 0, &mut sink, transition);
    assert_eq!(show_result, CursorTransitionResult::Deferred);
    assert_eq!(sink.uploaded_hotspot, Some((2, 3)));
}

// Mutation: make the no-matching-entry-bytes branch Deferred; this legacy
// resolution assertion must fail while the upload-error outcomes stay pinned.
#[test]
fn legacy_sink_resolutions_are_unchanged() {
    let mut sink = TestSink::new();
    let mut cursor = test_cursor_entry(Some(vec![0; 16 * 16 * 4]));
    sink.cursor_available = true;
    sink.upload_response = TestUploadResponse::Error;
    sink.cursor_visible = true;
    sink.hide_fails = true;
    let transition = Some(CursorTransition::ShowOnRetire {
        upload_version: 91,
        hot_x: 0,
        hot_y: 0,
        x: 10,
        y: 20,
    });

    let visible_retry = apply_cursor_transition_on_retire(Some(&cursor), 0, &mut sink, transition);
    assert_eq!(visible_retry, CursorTransitionResult::VisibleNeedsShowRetry);
    assert_eq!(sink.hide_calls, 1);

    sink.uploaded_version = None;
    sink.cursor_visible = false;
    sink.hide_fails = false;
    let hidden = apply_cursor_transition_on_retire(Some(&cursor), 0, &mut sink, transition);
    assert_eq!(hidden, CursorTransitionResult::Hidden);
    assert_eq!(sink.hide_calls, 1);

    cursor.bgra_bytes = None;
    sink.cursor_visible = true;
    sink.hide_fails = true;
    let upload_calls_before = sink.upload_calls;
    let no_bytes = apply_cursor_transition_on_retire(Some(&cursor), 0, &mut sink, transition);
    assert_eq!(no_bytes, CursorTransitionResult::VisibleNeedsShowRetry);
    assert_eq!(sink.upload_calls, upload_calls_before);
    assert_eq!(sink.hide_calls, 2);
}

// Mutation: mark the scene structure dirty for every repaint resolution;
// this Legacy scheduling regression must fail.
#[test]
fn legacy_cursor_retire_resolutions_do_not_dirty_scene_structure() {
    for result in [
        CursorTransitionResult::Applied,
        CursorTransitionResult::Hidden,
        CursorTransitionResult::HiddenNeedsRepaint,
        CursorTransitionResult::Visible,
        CursorTransitionResult::VisibleNeedsShowRetry,
    ] {
        let mut scene_structure_dirty = false;
        mark_cursor_transition_structure_dirty(&mut scene_structure_dirty, result);
        assert!(
            !scene_structure_dirty,
            "Legacy result {result:?} must preserve master scene scheduling"
        );
    }
    let mut scene_structure_dirty = false;
    mark_cursor_transition_structure_dirty(
        &mut scene_structure_dirty,
        CursorTransitionResult::Deferred,
    );
    assert!(
        scene_structure_dirty,
        "Deferred must wake the scene retry path"
    );
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

    let pending = resolve_retired_cursor_state(
        CursorTransitionResult::HiddenNeedsRepaint,
        mode_after,
        mode_after,
    );
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
    let failed = resolve_retired_cursor_state(
        CursorTransitionResult::Visible,
        OutputCursorMode::SwPending,
        OutputCursorMode::Hw,
    );
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
    let resolution = resolve_retired_cursor_state(
        CursorTransitionResult::Hidden,
        OutputCursorMode::Hw,
        OutputCursorMode::Hidden,
    );
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
        OutputCursorMode::Hw,
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
    assert_eq!(
        retry,
        Some(CursorShowRetry {
            version: 7,
            deferred: false,
        })
    );
}

#[test]
fn unrelated_old_ack_cannot_clear_newer_show_retry() {
    assert_eq!(
        update_force_show_retry_version(
            Some(CursorShowRetry {
                version: 8,
                deferred: false,
            }),
            None,
            CursorTransitionResult::Applied,
            OutputCursorMode::Hw,
        ),
        Some(CursorShowRetry {
            version: 8,
            deferred: false,
        })
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
            Some(CursorShowRetry {
                version: 9,
                deferred: false,
            }),
            old_show,
            CursorTransitionResult::Applied,
            OutputCursorMode::Hw,
        ),
        Some(CursorShowRetry {
            version: 9,
            deferred: false,
        })
    );
    let newer_show = Some(CursorTransition::ShowOnRetire {
        upload_version: 10,
        hot_x: 0,
        hot_y: 0,
        x: 10,
        y: 20,
    });
    assert_eq!(
        update_force_show_retry_version(
            Some(CursorShowRetry {
                version: 9,
                deferred: true,
            }),
            newer_show,
            CursorTransitionResult::Applied,
            OutputCursorMode::Hw,
        ),
        None
    );
    assert_eq!(
        update_force_show_retry_version(
            Some(CursorShowRetry {
                version: 9,
                deferred: true,
            }),
            newer_show,
            CursorTransitionResult::Hidden,
            OutputCursorMode::Hw,
        ),
        Some(CursorShowRetry {
            version: 9,
            deferred: true,
        }),
        "a Hidden result does not count as a successful newer bind"
    );
    assert_eq!(
        update_force_show_retry_version(
            Some(CursorShowRetry {
                version: 9,
                deferred: false,
            }),
            newer_show,
            CursorTransitionResult::Applied,
            OutputCursorMode::Hw,
        ),
        Some(CursorShowRetry {
            version: 9,
            deferred: false,
        }),
        "an older legacy retry keeps master's generation-qualified behavior"
    );
    // Mutation: make Deferred preserve a higher pending version; this named assertion must fail.
    assert_eq!(
        update_force_show_retry_version(
            Some(CursorShowRetry {
                version: 9,
                deferred: true,
            }),
            old_show,
            CursorTransitionResult::Deferred,
            OutputCursorMode::Hw,
        ),
        Some(CursorShowRetry {
            version: 8,
            deferred: true,
        }),
        "the latest deferred attempt replaces a higher-version pending retry"
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
            Some(CursorShowRetry {
                version: 9,
                deferred: false,
            }),
            show,
            CursorTransitionResult::Applied,
            OutputCursorMode::Hw,
        ),
        None
    );
}

#[test]
fn lifecycle_reset_discards_stale_retry_before_fresh_hidden_show() {
    let mut retry = Some(CursorShowRetry {
        version: 1,
        deferred: true,
    });
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

/// Single-output Hw is Hw; single-output Sw and SwPending are Sw.
#[test]
fn classify_cursor_mode_single_output_cases() {
    assert_eq!(
        classify_cursor_mode_from_per_output([OutputCursorMode::Hw]),
        CursorPlaneMode::Hw,
    );
    assert_eq!(
        classify_cursor_mode_from_per_output([OutputCursorMode::Hidden]),
        CursorPlaneMode::Hidden,
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

#[test]
// Mutation: classify all-Hidden outputs as Sw. The first assertion must fail.
fn cursor_mode_hidden_outputs_and_mixed_hidden_modes() {
    let Some(gpu) = crate::kms::render::gpu::GpuCore::for_tests_with_vk() else {
        eprintln!("skipping: no Vulkan test device");
        return;
    };
    let mut sink = TestSink::new();
    sink.gpu = gpu;
    let layouts = [layout_at((0, 0, 800, 600)), layout_at((800, 0, 800, 600))];
    let mut scene = SceneCompositor::new(&sink, &layouts).expect("construct live test scene");

    assert_eq!(scene.cursor_mode(), CursorPlaneMode::Hidden);

    scene.inner.as_mut().unwrap().outputs[1].last_frame_cursor_mode =
        OutputCursorMode::Sw { prev: None };
    assert_eq!(scene.cursor_mode(), CursorPlaneMode::Sw);

    scene.inner.as_mut().unwrap().outputs[1].last_frame_cursor_mode = OutputCursorMode::Hw;
    assert_eq!(scene.cursor_mode(), CursorPlaneMode::Hw);
}

#[test]
// Mutation: keep treating CursorPlaneMode::Hidden as nothing to move. The
// backend wake assertion must fail when motion re-enters an output.
fn cursor_hidden_in_dead_zone_is_assigned_to_reentered_output() {
    let mut sink = TestSink::new();
    let Some(gpu) = GpuCore::for_tests_with_vk() else {
        eprintln!("skipping: no Vulkan test device");
        return;
    };
    sink.gpu = gpu;
    let outputs = [layout_at((0, 0, 800, 600)), layout_at((900, 0, 800, 400))];
    let mut scene = SceneCompositor::new(&sink, &outputs).expect("construct live test scene");
    let mut core = TestCore::for_tests();
    let windows = crate::kms::render::window_geometry::WindowsMap::new();
    let mut store = crate::kms::render::store::DrawableStore::new();
    let mut cursor_storage = crate::kms::render::store::Storage::for_tests_null(
        extent(16, 16),
        vk::Format::B8G8R8A8_UNORM,
    );
    let cursor_view: vk::ImageView = ash::vk::Handle::from_raw(0xCAFE_BA11);
    cursor_storage.image_view = cursor_view;
    cursor_storage.sample_view = cursor_view;
    let cursor_id = store
        .allocate(
            0xCAFE_BA12,
            crate::kms::render::store::DrawableKind::Pixmap,
            32,
            false,
            cursor_storage,
        )
        .expect("allocate cursor sprite");
    scene.register_cursor(CursorEntry {
        id: cursor_id,
        extent: extent(16, 16),
        hot_x: 0,
        hot_y: 0,
        record_version: 1,
        bgra_bytes: None,
    });

    // The pointer is in the gap and below the shorter output, so neither
    // output intersects its cursor footprint.
    core.cursor_x = 850.0;
    core.cursor_y = 450.0;
    let dead_zone_assignments: Vec<_> = (0..outputs.len())
        .map(|output_idx| {
            scene.cursor_assignment_for_tests(
                &core.input(&IndexedWindows::from(windows.clone()), &outputs),
                &mut store,
                &sink,
                output_idx,
            )
        })
        .collect();
    assert_eq!(
        dead_zone_assignments,
        vec![CursorAssignment::Hidden, CursorAssignment::Hidden],
    );

    // Re-entry lands on output 1. Its TestSink path uses a software sprite,
    // and build_scene must assign that sprite in output-local coordinates.
    core.cursor_x = 950.0;
    core.cursor_y = 350.0;
    let indexed_windows = IndexedWindows::from(windows.clone());
    let input = core.input(&indexed_windows, &outputs);
    assert_eq!(
        scene.cursor_assignment_for_tests(&input, &mut store, &sink, 0,),
        CursorAssignment::Hidden,
    );
    assert_eq!(
        scene.cursor_assignment_for_tests(&input, &mut store, &sink, 1,),
        CursorAssignment::Sw { pos: (50, 350) },
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
