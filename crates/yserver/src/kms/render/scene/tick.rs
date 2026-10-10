use super::*;

impl TickOutcome {
    pub(super) fn clears_scene_structure_dirty(self) -> bool {
        matches!(
            self,
            Self::Composed | Self::Skipped(TickSkipReason::EmptyDamage)
        )
    }

    /// True iff `build_scene` ran for this output (so its presented and
    /// pieces ids were recorded and its `last_pieces` refreshed). The
    /// `PendingAcks`/`RetryDeadline`/`NothingPending` skips return BEFORE
    /// the walk; everything else runs it. `tick` reconciles dormancy on any
    /// tick where some output walked; the outputs that did not walk speak
    /// through their retained `last_pieces` — see `dormancy_inputs`.
    pub(super) fn walked(self) -> bool {
        !matches!(
            self,
            Self::Skipped(TickSkipReason::PendingAcks)
                | Self::Skipped(TickSkipReason::RetryDeadline)
                | Self::Skipped(TickSkipReason::NothingPending)
        )
    }
}

impl SceneCompositor {
    /// Earliest pending commit-retry deadline across outputs.
    pub(crate) fn earliest_retry_deadline(&self) -> Option<std::time::Instant> {
        self.inner
            .as_ref()?
            .outputs
            .iter()
            .filter_map(|o| o.next_submit_retry_at)
            .min()
    }

    /// Whether a dirty scene can submit to at least one output now.
    ///
    /// `maybe_composite` uses this before flushing deferred paint
    /// batches. If every output is still waiting on a pageflip or a
    /// commit-retry backoff, flushing paint would create GPU submit
    /// traffic that cannot be scanned out yet and would fragment COW
    /// batching under compositor drag workloads.
    pub(crate) fn has_output_ready_for_submit(&self) -> bool {
        let Some(inner) = self.inner.as_ref() else {
            return true;
        };
        let now = std::time::Instant::now();
        inner.outputs.iter().any(|o| {
            o.pending_acks.is_empty()
                && o.next_submit_retry_at
                    .is_none_or(|deadline| now >= deadline)
        })
    }

    /// True if any output's per-BO damage model owes a repaint that no producer
    /// is reporting — after an `invalidate`, or a submitted frame that never
    /// reached the screen. The backend's compose-wanted predicate must include
    /// this, or an invalidated output waits for unrelated damage to be repaired.
    pub(crate) fn owes_repaint(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|inner| inner.outputs.iter().any(|o| o.damage.owes_repaint()))
    }

    /// Compose a frame per output. Each output that has a free
    /// BO produces one atomic flip. Returns the number of
    /// output indices that successfully submitted (empty if everything was
    /// stalled / not dirty / no scene entries).
    ///
    /// # Errors
    ///
    /// Per-output failures don't abort the loop; they're logged
    /// and the next output is attempted. Top-level Err means
    /// the platform was unusable (no Vk).
    pub(crate) fn tick(
        &mut self,
        core: &KmsCore,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        windows: &crate::kms::render::backend::WindowsMap,
        telemetry: &mut Telemetry,
        cow_host_xid: Option<u32>,
    ) -> Result<Vec<usize>, SceneError> {
        // Destructure so `inner` (mutable) and `root_overlay` (shared)
        // are borrowed as disjoint fields: `tick_one_output` needs
        // `&mut inner` for the overlay-XOR pipeline cache AND read
        // access to the retained root overlay living on the outer struct.
        let SceneCompositor {
            inner,
            root_overlay,
            scene_structure_dirty,
            ..
        } = self;
        let Some(inner) = inner.as_mut() else {
            return Err(SceneError::NoVk);
        };
        platform.refresh_fence_pool_failure();
        if platform.renderer_failed {
            return Ok(Vec::new());
        }
        debug_assert_eq!(
            inner.outputs.len(),
            platform.outputs.len(),
            "scene/platform output vectors must stay in lockstep",
        );
        let mut composed = Vec::new();
        let mut clear_dirty = true;
        // Idle free-run fix (cut 2b): union of sampled sources drawn
        // across all outputs, and whether every output actually walked
        // `build_scene`. Only reconcile `offscreen_no_draw` when all
        // walked — see `TickOutcome::walked`.
        let mut drawn: std::collections::HashSet<crate::kms::render::store::DrawableId> =
            std::collections::HashSet::new();
        let mut had_pieces: std::collections::HashSet<crate::kms::render::store::DrawableId> =
            std::collections::HashSet::new();
        // Which outputs ran `build_scene` this tick. Dormancy is reconciled on
        // any tick where at least one did; outputs that did not walk
        // contribute their retained `last_pieces` instead (see
        // `dormancy_inputs`). Requiring EVERY output to walk (the old rule)
        // meant that with two outputs and one of them skipping as
        // `NothingPending`, reconciliation never ran at all and nothing ever
        // went dormant — the root's covered damage was re-peeked and
        // re-classified Hidden ~1000×/s (silence/MATE, 2026-09-04).
        let mut walked_outputs: Vec<bool> = vec![false; inner.outputs.len()];
        let mut carried = Vec::new();
        if let Err(e) = ensure_intermediates(inner, platform) {
            log::warn!("render scene tick: transform intermediate allocation failed: {e}");
        }
        let hw_cursor = hw_cursor_allowed(platform);
        if damage_audit_enabled() {
            emit_damage_audit_heartbeat(inner);
        }
        // Inputs of the pre-walk predicate that are global to the tick, read
        // once: the structure flag, and whether any armed drawable has
        // presentation damage waiting (an O(drawables) scan with an early exit
        // — far cheaper than one walk, let alone one per output).
        let structure_dirty = *scene_structure_dirty;
        // Per output: can an armed, damaged drawable land here? Decided from
        // each output's retained `last_pieces` — see
        // `pending_presentation_for_output`. Computed for every output up
        // front because the loop below borrows `inner` mutably.
        let pending_presentation_per_output: Vec<bool> = {
            let armed = store.armed_damaged_ids();
            let all: Vec<&std::collections::HashSet<crate::kms::render::store::DrawableId>> =
                inner.outputs.iter().map(|o| &o.last_pieces).collect();
            inner
                .outputs
                .iter()
                .map(|o| pending_presentation_for_output(&armed, &o.last_pieces, &all))
                .collect()
        };
        for (output_idx, &pending_presentation) in
            pending_presentation_per_output.iter().enumerate()
        {
            // The union of the OTHER outputs' retained pieces, read now rather
            // than before the loop so an output walked earlier in this same
            // iteration contributes its fresh set.
            let elsewhere: std::collections::HashSet<crate::kms::render::store::DrawableId> = inner
                .outputs
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != output_idx)
                .flat_map(|(_, o)| o.last_pieces.iter().copied())
                .collect();
            match tick_one_output(
                inner,
                output_idx,
                core,
                store,
                platform,
                windows,
                telemetry,
                hw_cursor,
                cow_host_xid,
                root_overlay,
                &elsewhere,
                &mut drawn,
                &mut had_pieces,
                &mut carried,
                structure_dirty,
                pending_presentation,
            ) {
                Ok(outcome) => {
                    if outcome == TickOutcome::Composed {
                        composed.push(output_idx);
                        // The other outputs' damage is handed over as
                        // structure damage; keep the scheduler awake for it.
                        if fan_out_carried_damage(inner, output_idx, &carried) {
                            clear_dirty = false;
                        }
                    } else {
                        clear_dirty &= outcome.clears_scene_structure_dirty();
                    }
                    walked_outputs[output_idx] = outcome.walked();
                }
                Err(e) => {
                    clear_dirty = false;
                    log::warn!(
                        "render scene tick: output {output_idx} compose failed: {e}; continuing",
                    );
                }
            }
        }
        if walked_outputs.iter().any(|w| *w) {
            let none = std::collections::HashSet::new();
            let reports: Vec<OutputWalkReport<'_>> = inner
                .outputs
                .iter()
                .zip(&walked_outputs)
                .map(|(o, &walked)| OutputWalkReport {
                    walked,
                    // `drawn` is the union over the walked outputs; for the
                    // union `dormancy_inputs` takes, attributing it to each
                    // walked output is equivalent.
                    presented: if walked { &drawn } else { &none },
                    last_pieces: &o.last_pieces,
                })
                .collect();
            let (keep_armed, pieces_anywhere) = dormancy_inputs(&reports);
            debug_assert!(
                had_pieces.iter().all(|id| pieces_anywhere.contains(id)),
                "a walked output's pieces are its retained last_pieces"
            );
            let changed = store.reconcile_offscreen_no_draw(&keep_armed, &pieces_anywhere);
            // A window whose damage is pending while it is wrongly dormant is
            // stranded: `NoPieces` never re-arms on paint, so its content only
            // heals where something else composes. Log every transition behind
            // the tick-skip gate so a hardware log can name the drawable
            // instead of leaving the verdict to inference.
            if tick_skip_log_enabled() {
                for (id, reason) in changed {
                    log::info!("dormant-diag: drawable={id:?} reason={reason:?}");
                }
            }
        }
        if clear_dirty {
            *scene_structure_dirty = false;
        }
        // Vulkan may export the already-signalled SYNC_FD sentinel (`fd=-1`).
        // Such a job has no pollable fd, so drain only after every composed
        // output installed its PendingAck; exact job/BO matching is then live
        // before the immediate B submission runs.
        for completion in platform.drain_scanout_render_completions() {
            if platform.renderer_failed {
                break;
            }
            if !handle_scanout_render_completion_inner(inner, completion, platform) {
                telemetry.record_missed_pageflip();
            }
            if platform.renderer_failed {
                break;
            }
        }
        Ok(composed)
    }
}

pub(super) fn tick_skip_log_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        matches!(
            std::env::var("YSERVER_TICK_SKIP_LOG").ok().as_deref(),
            Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
        )
    })
}

/// Diagnostic: record that `tick_one_output` skipped this output at
/// `reason`. Logs at INFO **only on transition** (different reason
/// from the previous tick, or first skip after a successful flip),
/// keeping the log volume bounded at one line per skip-state change
/// per output. Used to debug "output stops getting page-flips
/// indefinitely" by identifying which gate is stuck. Gated OFF by
/// default — see [`tick_skip_log_enabled`].
fn record_tick_skip(
    state: &mut OutputSceneState,
    output_idx: usize,
    reason: TickSkipReason,
    output_damage_rects: usize,
) {
    if !tick_skip_log_enabled() {
        return;
    }
    if state.last_skip_reason != Some(reason) {
        log::info!(
            "render scene tick skip: output={output_idx} reason={reason:?} \
             pending_acks={pa} retry_at={ra:?} damage_rects={dr} \
             scene_structure_rects={ssr} prev_reason={pr:?}",
            pa = state.pending_acks.len(),
            ra = state.next_submit_retry_at,
            dr = output_damage_rects,
            ssr = state.scene_structure_damage.rects().len(),
            pr = state.last_skip_reason,
        );
        state.last_skip_reason = Some(reason);
    }
}

/// Diagnostic: record that `tick_one_output` succeeded (composed +
/// submitted) for this output, clearing any prior skip state. Logs
/// at INFO **only if we were previously skipping**, marking the
/// "unblocked" transition for the freeze-debug timeline.
fn record_tick_success(state: &mut OutputSceneState, output_idx: usize) {
    if !tick_skip_log_enabled() {
        return;
    }
    if let Some(prev) = state.last_skip_reason.take() {
        log::info!(
            "render scene tick unblock: output={output_idx} prev_reason={prev:?} composed=ok",
        );
    }
}

/// True if any captured presentation-damage snapshot carries a
/// NON-EMPTY region. Gates the empty-projection force-compose in
/// `tick_one_output`: `peek_presentation_damage` returns `Some` even
/// for a clean (empty) region, so a mere `!snapshots.is_empty()` check
/// force-composes the whole output for every drawn window every vblank
/// — the idle free-run bug. Only a window that actually painted
/// (non-empty captured damage) whose projection landed empty needs the
/// forced full compose (the xfce submenu case).
/// Whether a tick with no output damage may skip composing.
///
/// `owed` is what the per-BO damage model still has to paint regardless of
/// producers — set by `ScanoutDamage::invalidate` (a truncated submit, a failed
/// flip, a return from direct scanout, a drain) and by a frame whose submit
/// succeeded but never reached the screen. Before this was consulted here, an
/// invalidated output with no fresh damage stayed on its stale frame until
/// unrelated damage arrived, because this skip ran before the plan ever looked
/// at the model (codex, post-merge review of `02bafec3`, finding 4).
pub(super) fn skip_for_empty_damage(damage_empty: bool, first_frame: bool, owed: bool) -> bool {
    damage_empty && !first_frame && !owed
}

/// Whether this tick has to walk the scene at all.
///
/// The walk is how the tick learns whether anything is damaged, so until now
/// every wake walked every output and then, most of the time, took the
/// `EmptyDamage` skip. On the e16 phased workload that was 769 walks/s at 57
/// composes/s (13.8 per compose, 436 µs each — a third of a core), because
/// e16's pager copies ~1000 strips/s and every paint wakes the loop, and the
/// present-completion poll wakes it every millisecond besides.
///
/// Everything `build_scene` can discover is announced beforehand by one of
/// these inputs, so a tick where none is set cannot produce damage and may skip
/// without walking:
///
/// - `structure_dirty` — `scene_structure_dirty`: every mutation of a scene
///   input the walk reads (geometry, map state, stacking, shape, redirect
///   routing, storage reallocation, cursor image or software-cursor position,
///   root overlay, output topology, direct-scanout transitions) calls
///   `wake_for_damage` or `mark_scene_structure_*`. A hardware-cursor move
///   goes straight to the plane and correctly does not set it.
/// - `pending_presentation` — an ARMED drawable painted, was not acked, and
///   can land on THIS output: see [`pending_presentation_for_output`]. Dormant
///   drawables (`DormantReason`) are excluded by design; a `HiddenDamage` one
///   re-arms on its next paint. The global form of this input had no effect
///   on e16 (2 outputs): the pager paints ~1000/s on output 0, so some armed
///   drawable is always damaged, and the wake walked output 1 — where nothing
///   is — ~700 times a second to find `EmptyDamage`.
/// - `first_frame` — nothing has ever been presented on this output.
/// - `owed` — the per-BO damage model still owes a repaint (`invalidate`,
///   a frame that never reached the screen).
/// - `pending_structure` — this output already holds rect-precise structure
///   damage or a failed-submit repaint. Both setters also raise
///   `structure_dirty`; listed separately so the predicate does not depend on
///   that coupling.
/// - `audit_armed` — the damage audit uses the empty-damage path for its idle
///   re-compare and must keep walking.
///
/// A wake that a mutation site forgot shows up as a stale window; the audit
/// (`YSERVER_DAMAGE_AUDIT=1`) is what catches it, which is why it forces walks.
pub(super) fn walk_needed(
    structure_dirty: bool,
    pending_presentation: bool,
    first_frame: bool,
    owed: bool,
    pending_structure: bool,
    audit_armed: bool,
) -> bool {
    structure_dirty
        || pending_presentation
        || first_frame
        || owed
        || pending_structure
        || audit_armed
}

/// The two sets `DrawableStore::reconcile_offscreen_no_draw` needs, computed so
/// that reconciliation can run on any tick where at least one output walked.
///
/// A drawable with pending damage goes dormant iff, for EVERY output, either
/// that output walked this tick and did not present it, or that output did not
/// walk and the drawable is not in its retained `last_pieces`. The second clause
/// is what makes skipping safe: an output that did not walk (flip pending,
/// retry deadline, nothing pending) has unchanged visibility since its last
/// walk — every structural change forces a walk everywhere — so if the drawable
/// has pieces there, that output may present it once it walks, and its pre-walk
/// predicate WILL walk it, because the drawable is armed and in its
/// `last_pieces`. Until then it must stay armed. Returns `(keep_armed,
/// pieces_anywhere)`: ids that stay armed, and ids with pieces on some output
/// (which decides `HiddenDamage` vs `NoPieces` for the rest).
pub(super) fn dormancy_inputs(
    reports: &[OutputWalkReport<'_>],
) -> (
    std::collections::HashSet<crate::kms::render::store::DrawableId>,
    std::collections::HashSet<crate::kms::render::store::DrawableId>,
) {
    let mut keep_armed = std::collections::HashSet::new();
    let mut pieces_anywhere = std::collections::HashSet::new();
    for r in reports {
        if r.walked {
            keep_armed.extend(r.presented.iter().copied());
        } else {
            // Unchanged visibility since its last walk: whatever had pieces
            // there may still be presented there, once it walks.
            keep_armed.extend(r.last_pieces.iter().copied());
        }
        pieces_anywhere.extend(r.last_pieces.iter().copied());
    }
    (keep_armed, pieces_anywhere)
}

/// Whether any armed, damaged drawable can land on one output — the per-output
/// form of the `pending_presentation` input of [`walk_needed`].
///
/// `mine` is this output's retained `last_pieces`; `all` is every output's.
/// A drawable is pending for this output if it emitted pieces here in the
/// last walk, or if it emitted pieces on NO output — never walked, newly
/// created, or otherwise unknown — in which case every output walks. That is
/// the conservative direction: the cost of a wrong "yes" is one walk, the cost
/// of a wrong "no" is a stale window.
///
/// Why the sets are trustworthy without geometry: where a drawable is visible
/// changes only through a structural change, and every structural change sets
/// `scene_structure_dirty`, which forces the walk on its own. So whenever this
/// function is the deciding input, the sets are from a walk of the current
/// structure.
///
/// Multi-output: a drawable spanning both outputs is in both sets ⇒ both walk.
/// One on output 0 only ⇒ output 1 skips; its damage is captured by output 0's
/// compose and acked at that retire, as today. An off-output window emits no
/// pieces anywhere ⇒ in no set ⇒ every output walks, and today's
/// off-output force-compose path is preserved.
pub(super) fn pending_presentation_for_output(
    armed: &[crate::kms::render::store::DrawableId],
    mine: &std::collections::HashSet<crate::kms::render::store::DrawableId>,
    all: &[&std::collections::HashSet<crate::kms::render::store::DrawableId>],
) -> bool {
    armed
        .iter()
        .any(|id| mine.contains(id) || !all.iter().any(|set| set.contains(id)))
}

pub(super) fn snapshots_carry_damage(snaps: &[DamageSnapshot]) -> bool {
    snaps.iter().any(|s| !s.region.is_empty())
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
fn tick_one_output(
    inner: &mut SceneCompositorInner,
    output_idx: usize,
    core: &KmsCore,
    store: &mut DrawableStore,
    platform: &mut PlatformBackend,
    windows: &crate::kms::render::backend::WindowsMap,
    telemetry: &mut Telemetry,
    hw_strategy_enabled: bool,
    cow_host_xid: Option<u32>,
    root_overlay: &crate::kms::render::root_overlay::RootOverlay,
    // Idle free-run fix (cut 2b): accumulator for the sampled-source
    // ids `build_scene` actually drew on this output, unioned across
    // outputs by `tick` to reconcile `offscreen_no_draw`. Only written
    // once `build_scene` has run (after the pending-flip/retry gate),
    // so a `PendingAcks`/`RetryDeadline` skip contributes nothing.
    // What the OTHER outputs showed at their last walk, for classifying an
    // off-output paint as theirs (`ContentDamage::OtherOutput`) rather than
    // stranded. See `WalkSink::elsewhere`.
    elsewhere: &std::collections::HashSet<crate::kms::render::store::DrawableId>,
    drawn: &mut std::collections::HashSet<crate::kms::render::store::DrawableId>,
    // Sampled sources that emitted at least one piece on this output — with
    // `drawn` this picks the dormancy reason (see `DormantReason`).
    had_pieces: &mut std::collections::HashSet<crate::kms::render::store::DrawableId>,
    // This output's carried content damage in root coordinates, replaced on a
    // compose (`Composed`) for `tick` to fan out to the other outputs.
    carried: &mut Vec<CarriedDamage>,
    // Pre-walk predicate inputs read once per tick by `tick` — see
    // `walk_needed`.
    structure_dirty: bool,
    pending_presentation: bool,
) -> Result<TickOutcome, SceneError> {
    // 0. **Per-output flip-pending gate.** KMS only allows one
    //    pending atomic commit per CRTC at a time; a second
    //    `drmModeAtomicCommit` while the first hasn't fired
    //    page-flip-complete returns EBUSY. Without this check
    //    the loop fires submit-after-submit faster than vblank,
    //    every second submit takes the 9b recovery path
    //    (BO invalidated, repaint deferred), nothing ever
    //    actually displays. Observed catastrophic on RADV/bee
    //    + mate + 2560x1440 (screen stays at the initial
    //    pageflip frame; bg_pixel-unset = black).
    //
    //    Skip cleanly: pending_ack non-empty means a flip is in
    //    flight. `scene_structure_dirty` stays set so the next
    //    tick (post-page-flip-complete) picks up the deferred
    //    damage. The KMS-rate cap is now structural; the rest
    //    of the pipeline can fire at whatever cadence
    //    `maybe_composite` calls us — wasted cycles bounded
    //    here.
    {
        let vk = Arc::clone(&inner.vk);
        let s = inner.outputs.get_mut(output_idx).expect("range");
        retire_failed_submit_bos(s, output_idx, platform, vk.as_ref());
        // B.2-context fix (vkdebug VUID-vkResetDescriptorPool-00313):
        // drain any deferred descriptor-pool slot releases whose
        // compose fence has now signaled. Deferred entries are
        // queued at `handle_page_flip_complete` when the GPU hadn't
        // yet finished the compose CB at KMS pageflip time;
        // releasing the slot then would have tripped the VUID.
        drain_pending_pool_releases(s, vk.as_ref(), platform);
        if !s.pending_acks.is_empty() {
            record_tick_skip(s, output_idx, TickSkipReason::PendingAcks, 0);
            return Ok(TickOutcome::Skipped(TickSkipReason::PendingAcks));
        }
        if let Some(deadline) = s.next_submit_retry_at
            && std::time::Instant::now() < deadline
        {
            record_tick_skip(s, output_idx, TickSkipReason::RetryDeadline, 0);
            return Ok(TickOutcome::Skipped(TickSkipReason::RetryDeadline));
        }
        // 0b. Pre-walk predicate. Everything `build_scene` could find is
        //     announced by one of these inputs; if none is set, walking would
        //     only end in the `EmptyDamage` skip below at the cost of the walk.
        if !walk_needed(
            structure_dirty,
            pending_presentation,
            s.current_generation == 0,
            s.damage.owes_repaint(),
            !s.scene_structure_damage.is_empty()
                || !s.pending_repaint_after_failed_submit.is_empty(),
            damage_audit_enabled(),
        ) {
            record_tick_skip(s, output_idx, TickSkipReason::NothingPending, 0);
            telemetry.record_tick_skip_nothing_pending();
            return Ok(TickOutcome::Skipped(TickSkipReason::NothingPending));
        }
    }

    // A transformed output composites its footprint into the intermediate
    // and scales that into the BO (spec D4).
    let transform = platform.output_transform(output_idx).cloned();
    if transform.is_some() && inner.outputs[output_idx].intermediate.is_none() {
        return Err(SceneError::NoIntermediate(output_idx));
    }

    // 1. Snapshot live output state so we can fold cleanly
    //    into pending_ack later (codex round 2 point 2 —
    //    transactional generation advance).
    let (scene_structure_snap, failed_repaint_snap, frame_gen, first_frame) = {
        let s = inner.outputs.get(output_idx).expect("range");
        (
            s.scene_structure_damage.snapshot(),
            s.pending_repaint_after_failed_submit.snapshot(),
            s.current_generation + 1,
            s.current_generation == 0,
        )
    };

    // 2. Build the scene + collect projected presentation damage.
    //    Stage 5 Phase C: build_scene returns a pure
    //    `CursorAssignment` decision; the actual transition queue +
    //    `cursor_prev_pos` advance happens transactionally below
    //    AFTER the per-output commit succeeds.
    let cursor_prev_pos_before = inner.outputs[output_idx].cursor_prev_pos;
    let last_present_cursor_rect = inner.outputs[output_idx].last_present_cursor_rect;
    let last_present_cursor_version = inner.outputs[output_idx].last_present_cursor_version;
    let hw_can_run = hw_strategy_enabled;
    let scene_prev_mode = inner.outputs[output_idx].last_frame_cursor_mode;
    // Phase 5.1 — `cow_host_xid` is threaded directly from the
    // backend's `cow_host_xid()` getter (the well-known protocol
    // constant whenever the overlay is materialized, else `None`).
    // It flags the COW top-level in the `top_level_order` walk so
    // its subtree inherits `alpha_passthrough`. The COW emits via
    // the normal recursion — there is no special post-walk append.
    // Step 1 — the walk was unmeasured: `compose_cb_record_ns` starts after
    // `build_scene` returns, and this is the pass whose cost grows with the
    // window count. Timed on every tick that gets this far, composed or not.
    let build_scene_start = std::time::Instant::now();
    let mut built = build_scene_with(
        core,
        store,
        windows,
        output_idx,
        platform,
        inner.cursor.clone(),
        cursor_prev_pos_before,
        cow_host_xid,
        hw_can_run,
        Visibility::On,
        elsewhere,
    );
    telemetry.record_build_scene_ns(
        u64::try_from(build_scene_start.elapsed().as_nanos()).unwrap_or(u64::MAX),
    );
    // Retain what emitted pieces on this output for the pre-walk predicate.
    // Recorded before any skip below so the set always reflects the most
    // recent walk, composed or not.
    {
        let s = &mut inner.outputs[output_idx];
        s.last_pieces.clear();
        s.last_pieces.extend(built.pieces_ids.iter().copied());
    }
    let prev_mode = effective_cursor_prev_mode(
        scene_prev_mode,
        platform.cursor_plane_visible_for_output(output_idx),
        built.cursor_assignment,
    );
    if cursorless_hide_frame_required(prev_mode, built.cursor_assignment) {
        // Two-phase Hw→Sw/Hidden handoff: the frame retired immediately
        // before hide must contain no software cursor. If the hide ioctl
        // fails, the old HW sprite remains the sole visible cursor; if it
        // succeeds, retirement forces a later SW repaint.
        built.omit_software_cursor_for_hide();
    }

    // Idle free-run fix (cut 2b): record the sampled sources whose pending
    // damage this output PRESENTED, so `tick` can reconcile
    // `offscreen_no_draw` from the union across outputs. Recorded
    // unconditionally here (before the empty-damage / BO / pool skips below)
    // so a window that WAS presented is never mis-flagged just because its
    // output later skips. `presented_ids`, not `sampled_ids`: a node sampled
    // but with all of its damage under a cover must NOT count, or the
    // scheduler never goes dormant (see `WalkSink::presented_ids`).
    drawn.extend(built.presented_ids.iter().copied());
    had_pieces.extend(built.pieces_ids.iter().copied());

    // Stage 5 Phase D — derive the per-output cursor transition
    // and new prev_pos from `built.cursor_assignment` and the
    // last-frame mode. Both are queued on the PendingAck below
    // and applied transactionally on successful retirement.
    let (mut cursor_transition_to_queue, cursor_prev_pos_after_retire, cursor_mode_after_retire) =
        derive_cursor_transition(prev_mode, built.cursor_assignment);
    if inner.outputs[output_idx].force_show_retry_version.is_some()
        && let CursorAssignment::Hw {
            x,
            y,
            record_version,
            hot_x,
            hot_y,
        } = built.cursor_assignment
    {
        cursor_transition_to_queue = Some(CursorTransition::ShowOnRetire {
            upload_version: record_version,
            hot_x,
            hot_y,
            x,
            y,
        });
    }

    let mut output_damage = built.projected_damage;
    output_damage.union_with(&cursor_damage_for_frame(
        last_present_cursor_rect,
        last_present_cursor_version,
        built.new_cursor_rect,
        built.cursor_record_version,
        cursor_transition_to_queue,
    ));
    // Always-Full repaint makes stationary SW cursors safe even when
    // cursor_damage is empty and some unrelated damage triggers a
    // frame. If `Repaint::Clipped` is ever re-enabled, the current SW
    // cursor rect must also be folded into the repaint region even
    // when it did not itself trigger the compose.
    output_damage.union_with(&scene_structure_snap);
    output_damage.union_with(&failed_repaint_snap);

    // Step 2 — structural damage from diffing this frame's participants against
    // the last presented ones. Folded in HERE, before the empty-damage check
    // below: once 2b demotes the `mark_scene_structure_dirty` sites to a bare
    // wake, this is the ONLY thing that will report a map, unmap, restack or
    // drag. If it landed after that check, such a frame would find
    // `output_damage` empty, take the EmptyDamage skip, and the window would
    // never appear — a functional break, not a performance one.
    //
    // 2a keeps the whole-output hammer in place, so this can only ever add
    // damage the hammer already covers; it is exercised without being relied on.
    let structural = structural_damage(
        &inner.outputs[output_idx].prev_presented,
        &built.participants,
    );
    // Overdraw: summed draw area over output area, on the emitted draw list
    // before the scissor cull — how much the scene overpaints, not how much
    // survives a scissor. Summing clipped rect areas rather than unioning them
    // is deliberate: the union is the output area anyway, because the root
    // covers it.
    //
    // Computed here but RECORDED only on a frame that composes (beside
    // `record_damage_pixels`, which supplies the denominator). The walk runs on
    // every wake — ~11 per compose on silence/MATE — so recording it per walk
    // inflated `overdraw` by the walks-per-compose ratio: the "25×" measured on
    // 2026-09-02 was ~2× overdraw times ~11 walks per compose. Caught on the
    // z400 on 2026-09-03, where a startup bucket read 2284 with one compose.
    let scene_draw_pixels: u64 = built
        .scene
        .draws
        .iter()
        .filter_map(draw_dst_rect_inward)
        .filter_map(|r| clip_rect_to_output_extent(r, inner.outputs[output_idx].output_extent))
        .map(|r| u64::from(r.extent.width) * u64::from(r.extent.height))
        .sum();
    telemetry.record_structural_damage_pixels(structural.area());
    for rect in structural.rects() {
        output_damage.add(rect);
    }
    // Step 1 — nodes the walk visited vs draws it emitted after visibility
    // clipping (pre-scissor). These used to both be `draws.len()`.
    telemetry.record_scene_entries(built.stats.nodes_visited, built.stats.draws_emitted);
    telemetry.record_visibility(
        [
            built.stats.collapses_mine,
            built.stats.collapses_claim,
            built.stats.collapses_taken,
            built.stats.collapses_taken_skipped,
        ],
        built.stats.hidden_participants,
    );
    telemetry.record_content_damage(
        built.stats.content_hidden,
        built.stats.content_off_output,
        built.stats.content_other_output,
    );

    // 3. Empty-damage fast path (after first frame).
    if skip_for_empty_damage(
        output_damage.is_empty(),
        first_frame,
        inner.outputs[output_idx].damage.owes_repaint(),
    ) {
        // A drawn, scene-participating window had presentation damage
        // that `build_scene` CAPTURED (`built.snapshots`, via
        // `peek_presentation_damage`) but whose `add_projected_damage`
        // landed empty — a geometry/offset gap projecting a top-level
        // popup's damage off the output. Skipping here would DISCARD
        // those snapshots (they only ack via the `PendingAck` built at
        // the compose path below), so the paint would never ack and
        // the window sits painted-but-off-screen forever until an
        // unrelated event forces structure damage. This was the xfce
        // "submenu painted but not shown until you move" bug.
        //
        // Force a full-output repaint so the compose runs and the
        // captured snapshots ack at retire. Repaint is always-Full, so
        // the content composites correctly regardless of the empty
        // projection. Self-limiting: once acked, no snapshot carries
        // non-empty damage and the normal skip resumes → true idle.
        //
        // Gate on a snapshot with NON-EMPTY captured damage, NOT merely
        // `!built.snapshots.is_empty()`: `peek_presentation_damage`
        // returns `Some` even for an empty region (store.rs), so
        // `built.snapshots` is non-empty for EVERY drawn
        // scene-participating window — including perfectly clean idle
        // ones. Gating on non-emptiness was the idle free-run bug: a
        // clean drawn window force-composed the whole output every
        // vblank forever. Only a window that actually painted (non-empty
        // captured damage) whose projection landed empty needs the
        // force (the xfce submenu case).
        // DIAG (submenu regression, bee/eiger/air): the empty-damage
        // path with snapshots is exactly where cut 1's region-gate
        // decides force-vs-skip. Log what build_scene captured so we
        // can see the submenu's actual snapshot state at the failing
        // tick (present-but-empty region vs absent). Gated behind
        // YSERVER_TICK_SKIP_LOG like the other tick diagnostics — it
        // fires on EVERY empty-damage tick (~tens/s at idle), so leaving
        // it unconditional floods the log and allocates a Vec per tick,
        // defeating the idle goal.
        if tick_skip_log_enabled() {
            log::info!(
                "empty-damage-diag: out{output_idx} draws={} carry={} hidden={} off_output={} \
                 snapshots={:?}",
                built.scene.draws.len(),
                snapshots_carry_damage(&built.snapshots),
                built.stats.content_hidden,
                built.stats.content_off_output,
                built
                    .snapshots
                    .iter()
                    .map(|s| (s.id.as_u64(), s.region.rects().len()))
                    .collect::<Vec<_>>(),
            );
        }
        // Stage C — only damage that projected entirely OFF the output forces
        // a compose here. Damage that landed on the output but under a cover
        // (`ContentDamage::Hidden`) skips like clean idle: nothing on screen
        // changed, and the snapshot is re-peeked next walk. See
        // `ContentDamage` for why it is not acked either.
        if !built.stats.off_output_damage_forces_compose() {
            // Audit on the empty-damage path when a transition woke the
            // scene and reported nothing (the archetype bug), OR when the
            // idle re-compare is due. Without the second condition a
            // quiet desktop is never checked at all, so an unhealed
            // divergence would silently stop being reported — a clean
            // static soak would then mean nothing.
            if audit_has_unretired_event(inner, output_idx)
                || audit_idle_recompare_due(inner, output_idx)
            {
                let overlay_ops =
                    root_overlay.apply_list_for_output(platform.output_root_rect(output_idx));
                let (xor_pipeline, xor_layout) =
                    audit_overlay_pipeline(inner, !overlay_ops.is_empty())?;
                let reference = audit_reference_scene(
                    built.software_cursor_tail.is_some(),
                    core,
                    store,
                    windows,
                    output_idx,
                    platform,
                    inner.cursor.clone(),
                    cursor_prev_pos_before,
                    cow_host_xid,
                    hw_can_run,
                );
                run_damage_audit(
                    inner,
                    output_idx,
                    platform,
                    &built.scene,
                    reference.as_ref().map_or(&built.scene, |r| &r.scene),
                    &audit_sampled_pairs(store, &built.sampled_ids),
                    &output_damage,
                    None,
                    true,
                    &overlay_ops,
                    xor_pipeline,
                    xor_layout,
                )?;
            }
            let s = inner.outputs.get_mut(output_idx).expect("range");
            record_tick_skip(s, output_idx, TickSkipReason::EmptyDamage, 0);
            return Ok(TickOutcome::Skipped(TickSkipReason::EmptyDamage));
        }
        let extent = inner.outputs[output_idx].output_extent;
        output_damage.add(vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent,
        });
        log::debug!(
            "render: output {output_idx} forcing full compose — {} presentation-damage \
             snapshot(s) with real damage projected empty (paint would otherwise strand off-screen)",
            built
                .snapshots
                .iter()
                .filter(|s| !s.region.is_empty())
                .count(),
        );
    }

    // 3b. Step 3 — attribute this frame's damage.
    //
    // Placed HERE and not where `output_damage` is first assembled, because the
    // empty-damage block above can still inject a full-output rect when a
    // drawable carries real damage whose projection landed empty (the xfce
    // submenu case). Feeding the model before that would drop the injection.
    //
    // Shared outputs only: a copied (reverse-PRIME) output renders
    // `Repaint::Full` unconditionally and never consults this state, so it must
    // not accumulate any either.
    let shared_output = matches!(
        platform
            .scanout_pools
            .get(output_idx)
            .and_then(Option::as_ref),
        Some(OutputScanout::Shared(_))
    );
    // A transformed frame repaints the whole BO, so its damage (in
    // intermediate pixels) never enters the per-BO model.
    if shared_output && transform.is_none() {
        let mut damage_region = Region::from_rects(output_damage.rects().iter().copied());
        // Clip to the output. Damage outside it cannot be presented, and letting
        // it through would trip `commit_submitted`'s "painted covers repaint"
        // assertion — which compares against the full-output rect — turning a
        // stray rect from some producer into a debug-build panic on hardware.
        damage_region.intersect_rect(vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent: inner.outputs[output_idx].output_extent,
        });
        inner.outputs[output_idx].damage.add_damage(&damage_region);
    }

    // 4. Acquire BO.
    let token = match platform.acquire_scanout_bo(output_idx) {
        Some(t) => t,
        None => {
            let s = inner.outputs.get_mut(output_idx).expect("range");
            record_tick_skip(
                s,
                output_idx,
                TickSkipReason::NoBO,
                output_damage.rects().len(),
            );
            return Ok(TickOutcome::Skipped(TickSkipReason::NoBO));
        }
    };

    // 4b. Step 3 — what this BO is missing. Pure: acquiring mutates nothing, so
    // any later skip or failure leaves the model exactly as it was and the next
    // tick recomputes the same answer.
    //
    // `loadable` is the same condition `Repaint::Clipped` needs for a valid
    // `loadOp = LOAD`: the BO must have been through a present and not been
    // invalidated since. When false everything is missing, and step 4 must also
    // render Full — loading from a never-presented BO is invalid, not just stale.
    let bo_loadable = !token.content_invalidated && token.last_present_generation.is_some();
    let bo_repaint = if shared_output {
        inner.outputs[output_idx]
            .damage
            .repaint_for(token.bo_idx, bo_loadable)
    } else {
        Region::new()
    };

    // 5. Step 4 — decide how to repaint, and what that will paint.
    let extent = inner.outputs[output_idx].output_extent;

    // Two producers must be folded into the region before the decision: neither
    // is expressed as damage, and both are wrong under a scissor that misses
    // them.
    let mut requested = bo_repaint.clone();

    // The root `IncludeInferiors` XOR overlay is NOT idempotent. It is correct
    // today only because `Repaint::Full` CLEARs and fully redraws the BO, so the
    // overlay XORs exactly once onto fresh pixels. A clipped `loadOp = LOAD`
    // frame whose scissor misses the overlay rects would XOR them a SECOND time
    // onto a pooled BO that already has them baked in from a prior compose,
    // cancelling them or leaving remnants — the #90 rubber-band residual.
    for (_, rect) in root_overlay.apply_list_for_output(platform.output_root_rect(output_idx)) {
        requested.add_rect(rect);
    }

    // A stationary software cursor lives only in the BO that last drew it, so it
    // must be repainted even on a frame triggered by unrelated damage. Keyed off
    // the draw list rather than off the cursor assignment, which avoids two
    // mistakes: `new_cursor_rect` is `Some` for a HW-plane cursor too (that rect
    // is plane content, not BO content, and folding it would repaint a region on
    // every cursor move on the very path that exists to avoid that), and
    // `omit_software_cursor_for_hide` strips the SW draw for the Hw->Sw handoff
    // frame after the assignment was computed.
    if built.software_cursor_tail.is_some()
        && let Some(cursor_rect) = built.new_cursor_rect
    {
        requested.add_rect(cursor_rect);
    }
    requested.intersect_rect(vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent,
    });

    let plan = if transform.is_some() {
        RepaintPlan::full(extent, FullReason::Transformed)
    } else {
        plan_repaint(
            &requested,
            &built.scene.draws,
            extent,
            bo_loadable,
            shared_output,
        )
    };
    let repaint = plan.repaint;

    // The culled draw list is a SEPARATE product; `built.scene` stays whole for
    // the snapshots and the audit oracle. See `cull_scene_to_rect`.
    let culled = match repaint {
        Repaint::Clipped(_) => Some(cull_scene_to_region(&built.scene, &plan.painted)),
        Repaint::Full(_) | Repaint::AuditClearClipped(_) => None,
    };
    let render_scene: &CompositeScene = culled.as_ref().unwrap_or(&built.scene);
    if let (Repaint::Clipped(_), Some(c)) = (repaint, culled.as_ref()) {
        // Per scissor rect, not once against the bbox: with 4.5's per-rect
        // rendering different rects are legitimately covered by different
        // draws, and once step 1 fragments the root no single draw covers
        // anything that straddles a window edge.
        debug_assert!(
            plan.scissors
                .iter()
                .all(|r| opaque_cover_exists(&c.draws, *r)),
            "culling removed an opaque draw the clipped path depends on"
        );
    }

    match plan.full_reason {
        Some(reason) => {
            telemetry.record_full_redraw_fallback();
            telemetry.record_full_reason(match reason {
                FullReason::EmptyDrawList => "empty_draws",
                FullReason::UnloadableBo => "unloadable_bo",
                FullReason::NoOpaqueCover => "no_opaque_cover",
                FullReason::Threshold => "threshold",
                FullReason::CopiedRoute => "copied_route",
                FullReason::Transformed => "transformed",
            });
        }
        None => telemetry.record_clipped_repaint(),
    }
    // `damage_fraction` now reports what was actually rasterised, which is the
    // number that tracks GPU cost. The requested-region area is reported
    // separately, and the gap between them is bbox waste — the input to the
    // multi-rect decision in 4.5.
    telemetry.record_damage_pixels(
        plan.painted.area(),
        u64::from(extent.width) * u64::from(extent.height),
    );
    telemetry.record_damage_region_pixels(requested.area());
    // Same denominator as `damage_fraction`: one output area per compose.
    telemetry.record_scene_draw_pixels(scene_draw_pixels);

    let overlay_ops = root_overlay.apply_list_for_output(platform.output_root_rect(output_idx));
    let (xor_pipeline, xor_layout) = audit_overlay_pipeline(inner, !overlay_ops.is_empty())?;
    let reference = audit_reference_scene(
        built.software_cursor_tail.is_some(),
        core,
        store,
        windows,
        output_idx,
        platform,
        inner.cursor.clone(),
        cursor_prev_pos_before,
        cow_host_xid,
        hw_can_run,
    );
    run_damage_audit(
        inner,
        output_idx,
        platform,
        &built.scene,
        reference.as_ref().map_or(&built.scene, |r| &r.scene),
        &audit_sampled_pairs(store, &built.sampled_ids),
        &output_damage,
        None,
        false,
        &overlay_ops,
        xor_pipeline,
        xor_layout,
    )?;

    // 6. Acquire descriptor-pool slot.
    let state = inner.outputs.get_mut(output_idx).expect("range");
    let slot = match state.pool_ring.acquire() {
        Some(s) => s,
        None => {
            log::debug!(
                "render scene: descriptor-pool ring exhausted for output {output_idx}; skipping tick",
            );
            record_tick_skip(
                state,
                output_idx,
                TickSkipReason::NoPool,
                output_damage.rects().len(),
            );
            return Ok(TickOutcome::Skipped(TickSkipReason::NoPool));
        }
    };
    let descriptor_pool = state.pool_ring.pool_at(slot);

    // 7. Record + submit + flip via the v2 clipped compose path.
    let compose_ticket = match platform.acquire_fence_ticket() {
        Ok(ticket) => ticket,
        Err(error) => {
            inner.outputs[output_idx].pool_ring.release(slot);
            if vk_result_is_device_lost(error) {
                platform.renderer_failed = true;
            }
            return Err(SceneError::Present(PresentError::Vk(error)));
        }
    };
    let scale_pass = transform.as_ref().map(|t| {
        let intermediate = inner.outputs[output_idx]
            .intermediate
            .as_ref()
            .expect("checked at the top");
        let pipeline = inner
            .scale_pipeline
            .as_ref()
            .expect("an intermediate implies the pipeline");
        ScalePass::new(
            intermediate,
            pipeline,
            t,
            platform.output_root_rect(output_idx),
            (u32::from(platform.fb_w), u32::from(platform.fb_h)),
        )
    });
    // Root reads must not see a software cursor (`cursor_save`): the compose
    // saves the pixels under it in the image it writes.
    let compose_image = match (
        &scale_pass,
        platform
            .scanout_pools
            .get(output_idx)
            .and_then(|p| p.as_ref()),
    ) {
        (Some(sp), _) => Some(sp.image),
        (None, Some(OutputScanout::Shared(pool))) => {
            pool.bos.get(token.bo_idx).map(|bo| bo.vk_image)
        }
        (None, Some(OutputScanout::Copied(pool))) => {
            pool.sources.get(token.bo_idx).map(|source| source.image())
        }
        (None, None) => None,
    };
    let cursor_rect = built
        .software_cursor_tail
        .and(render_scene.draws.last())
        .and_then(draw_dst_rect_inward)
        .and_then(|rect| clip_rect_to_output_extent(rect, extent));
    let cursor_save = compose_image.and_then(|image| {
        inner.outputs[output_idx]
            .cursor_saves
            .prepare(&inner.vk, image, cursor_rect)
    });
    let output_key = platform.outputs[output_idx].key.clone();
    let drm_device = platform
        .device_for_output(&output_key)
        .map(|device| device.device.clone())
        .ok_or_else(|| {
            SceneError::Present(PresentError::Io(std::io::Error::other(format!(
                "no DRM device for output {:?}",
                output_key
            ))))
        })?;
    let pool = platform
        .scanout_pools
        .get_mut(output_idx)
        .and_then(|p| p.as_mut())
        .ok_or(SceneError::NoVk)?;
    // Retained root-`IncludeInferiors` overlay: per-output apply list
    // (output-local XOR rects), computed against the SAME per-output
    // layout the compose uses. Empty in the common case (no active
    // wireframe / rubber-band), so no XOR pipeline is built.
    let layout = &platform.outputs[output_idx];
    // Fetch the XOR-logic-op pipeline + layout here (needs `&mut inner`
    // for the cache) so `record_command_buffer` receives ready-to-bind
    // Vulkan handles and never touches `inner`. Skip the build entirely
    // when there is nothing to draw.
    let (xor_pipeline, xor_layout) = if overlay_ops.is_empty() {
        (vk::Pipeline::null(), vk::PipelineLayout::null())
    } else {
        let pl = inner.overlay_xor_cache.get(
            yserver_core::backend::GcFunction::Xor,
            crate::kms::vk::logic_fill_pipeline::LogicFillChannels::Color,
        )?;
        (pl, inner.overlay_xor_cache.pipeline_layout())
    };
    let mut gpu_submitted = false;
    // Step 1 — whether every draw of `render_scene` was actually recorded.
    // Descriptor allocation `break`s on pool exhaustion and the recorder draws
    // only the allocated prefix; on the clipped path a frame that painted less
    // than it claims must not be staged as `painted`. The copied route renders
    // Full and re-clears every frame, so only the shared path reports it.
    let mut compose_complete = true;
    let record_start = std::time::Instant::now();
    let (render_result, previous_gpu_ns, copied_prepare_failed) = match pool {
        OutputScanout::Shared(pool) => {
            let bo = pool.bos.get_mut(token.bo_idx).ok_or(SceneError::NoVk)?;
            let result = submit_shared_scanout_frame(
                &inner.vk,
                &drm_device,
                &layout.output,
                bo,
                &inner.pipeline,
                descriptor_pool,
                render_scene,
                repaint,
                &plan.scissors,
                compose_ticket.fence(),
                &mut gpu_submitted,
                &overlay_ops,
                xor_pipeline,
                xor_layout,
                scale_pass.as_ref(),
                cursor_save,
            )
            .map(|submitted| {
                compose_complete = compose_submit_was_complete(submitted, render_scene.draws.len());
                None
            });
            (result, bo.last_gpu_render_ns.take(), false)
        }
        OutputScanout::Copied(pool) => {
            let source = pool.sources.get_mut(token.bo_idx).ok_or(SceneError::NoVk)?;
            let destination_state = &mut pool
                .destinations
                .bos
                .get_mut(token.bo_idx)
                .ok_or(SceneError::NoVk)?
                .state;
            let result = submit_copied_scanout_render(
                &inner.vk,
                source,
                destination_state,
                &inner.pipeline,
                descriptor_pool,
                render_scene,
                Repaint::Full(if transform.is_some() {
                    extent
                } else {
                    token.extent
                }),
                &[],
                compose_ticket.fence(),
                &mut gpu_submitted,
                &overlay_ops,
                xor_pipeline,
                xor_layout,
                scale_pass.as_ref(),
                cursor_save,
            );
            let copied_prepare_failed = result
                .as_ref()
                .is_err_and(CopiedRenderSubmitError::requires_fail_stop);
            (
                result
                    .map(Some)
                    .map_err(CopiedRenderSubmitError::into_present),
                source.last_gpu_render_ns.take(),
                copied_prepare_failed,
            )
        }
    };
    if let Some(image) = compose_image {
        // A compose that reached the GPU rewrote the image even when the
        // flip after it failed.
        inner.outputs[output_idx].cursor_saves.finish(
            image,
            cursor_save,
            gpu_submitted.then_some(&compose_ticket),
        );
    }
    if copied_prepare_failed {
        // Importing the retained B -> A completion consumes its sole payload.
        // A failed import therefore cannot be retried without fabricating the
        // external memory dependency; fail-stop before the source can be
        // reserved or reused again.
        platform.renderer_failed = true;
    }
    let compose_result = match render_result {
        Ok(Some(completion)) => platform
            .register_scanout_render_completion(output_key, token.bo_idx, completion)
            .map(|job_id| InFlightStage::WaitingForRenderCompletion { job_id })
            .map_err(PresentError::Io),
        Ok(None) => Ok(InFlightStage::KmsFlipPending),
        Err(error) => Err(error),
    };
    let record_ns = u64::try_from(record_start.elapsed().as_nanos()).unwrap_or(u64::MAX);
    telemetry.record_compose_cb_record_ns(record_ns);
    // GPU-render time from this paired target's PREVIOUS compose (timestamp
    // pool), read before this command buffer overwrites the query slots.
    if let Some(gpu_ns) = previous_gpu_ns {
        telemetry.record_gpu_render_ns(gpu_ns);
    }
    telemetry
        .record_descriptor_allocations(u64::try_from(render_scene.draws.len()).unwrap_or(u64::MAX));

    let state = inner.outputs.get_mut(output_idx).expect("range");
    match compose_result {
        Ok(stage) => {
            state.next_submit_retry_at = None;
            if let Some(intermediate) = state.intermediate.as_mut() {
                intermediate.has_content = true;
            }
            for id in &built.sampled_ids {
                store.touch_render_fence(*id, compose_ticket.clone());
            }
            state.pool_slots.push_back(slot);
            state.presented_epochs = built.snapshots.iter().map(|s| (s.id, s.epoch)).collect();
            *carried = std::mem::take(&mut built.carried);
            state.pending_acks.push_back(PendingAck {
                bo_idx: token.bo_idx,
                generation: frame_gen,
                stage,
                drawable_snapshots: built.snapshots,
                ticket: Some(compose_ticket),
                submitted_output_damage: output_damage,
                submitted_participants: built.participants,
                submitted_scene_structure_damage: scene_structure_snap,
                submitted_failed_repaint: failed_repaint_snap,
                cursor_transition: cursor_transition_to_queue,
                cursor_prev_pos_after_retire,
                cursor_mode_after_retire,
                last_present_cursor_rect_after_retire: built.new_cursor_rect,
                last_present_cursor_version_after_retire: built.cursor_record_version,
            });
            state.current_generation = frame_gen;
            // Step 3 — stage the frame that just succeeded. Deliberately here
            // and not at submit *attempt*: an attempt that failed never staged,
            // so `pending` was never taken and the next tick recomputes an
            // identical repaint with nothing to roll back.
            //
            // `painted` is the whole output because `pick_repaint_region` still
            // returns `Repaint::Full`; step 4 replaces it with what the recorder
            // actually covered. It must always be a superset of `bo_repaint` —
            // `commit_submitted` asserts exactly that.
            if shared_output && transform.is_some() {
                // The scale pass wrote every BO pixel.
                let whole = Region::from_rect(vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: token.extent,
                });
                stage_submitted_frame(
                    &mut state.damage,
                    compose_complete,
                    token.bo_idx,
                    &bo_repaint,
                    &whole,
                );
            } else if shared_output {
                stage_submitted_frame(
                    &mut state.damage,
                    compose_complete,
                    token.bo_idx,
                    &requested,
                    &plan.painted,
                );
                if !compose_complete {
                    // Once per output rather than per frame: a scene that
                    // overflows the pool does so every Full frame.
                    static WARNED: std::sync::atomic::AtomicU32 =
                        std::sync::atomic::AtomicU32::new(0);
                    let bit = 1u32 << (output_idx % 32);
                    if WARNED.fetch_or(bit, std::sync::atomic::Ordering::Relaxed) & bit == 0 {
                        log::warn!(
                            "render scene: output {output_idx} composed {} of {} draws \
                             (descriptor pool exhausted); BO state invalidated, next \
                             frame repaints in full",
                            render_scene
                                .draws
                                .len()
                                .min(MAX_DESCRIPTOR_SETS_PER_FRAME as usize),
                            render_scene.draws.len(),
                        );
                    }
                }
            }
            record_tick_success(state, output_idx);
            Ok(TickOutcome::Composed)
        }
        Err(e) => {
            if present_error_is_device_lost(&e) {
                // The live source renderer submitted or recorded this frame.
                // Neither shared nor copied source resources are reusable
                // after DEVICE_LOST; latch the same fatal renderer state used
                // by the engine before any retry bookkeeping runs.
                platform.renderer_failed = true;
            }
            if gpu_submitted {
                for id in &built.sampled_ids {
                    store.touch_render_fence(*id, compose_ticket.clone());
                }
                // Rendering reached GPU A, but a later export, completion-job
                // registration, or shared KMS commit failed. Keep the paired
                // resources fenced until A is done and make buffer-age state
                // conservative; no pageflip event will retire this frame.
                platform.invalidate_bo(output_idx, token.bo_idx);
                telemetry.record_missed_pageflip();
                log::warn!(
                    "render scene: post-render scanout handoff failed for output \
                     {output_idx} (bo {}): {e}; BO invalidated",
                    token.bo_idx,
                );
            } else {
                log::warn!(
                    "render scene: compose record/queue submit failed for output \
                     {output_idx} (bo {}): {e}",
                    token.bo_idx,
                );
            }
            // Both failure paths fold repaint forward and do NOT
            // push a pending_ack or advance current_generation.
            // If the GPU submission happened, keep the scanout BO
            // and descriptor-pool slot alive until the compose
            // fence signals: KMS rejected the flip, so no page-flip
            // event will retire those resources for us.
            // Re-borrow `state` after the platform.invalidate_bo
            // call (which took &mut platform).
            let state = inner.outputs.get_mut(output_idx).expect("range");
            // TODO(stage-5 perf): the 100 ms commit-retry back-off is
            // hardcoded. Empirically picked to be wide enough that
            // RADV/amdgpu releases pinned resources between attempts
            // (16 ms / one vblank was too tight under the ENOMEM
            // storm). Should become a tunable + observable via
            // telemetry (e.g. `commit_retry_backoff_ms` counter) so
            // per-driver tuning is possible without code edits.
            state.next_submit_retry_at =
                Some(std::time::Instant::now() + std::time::Duration::from_millis(100));
            if let Some(br) = output_damage.bounding_rect() {
                state.pending_repaint_after_failed_submit.add(br);
            }
            if gpu_submitted {
                state.failed_submit_bos.push_back(FailedSubmitBo {
                    bo_idx: token.bo_idx,
                    pool_slot: slot,
                    ticket: compose_ticket,
                });
            } else {
                state.pool_ring.release(slot);
                platform.cancel_scanout_bo_recording(output_idx, token.bo_idx);
            }
            Err(SceneError::Present(e))
        }
    }
}
