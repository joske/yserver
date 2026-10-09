use super::*;

// ---------------- Step 1: present_supersession_covers ----------------

#[test]
fn coverage_full_predecessor_under_full_successor_is_covered() {
    let successor = SupersessionFixture::new(2, 0x100)
        .geometry(0, 0, 100, 100)
        .pending();
    let predecessor = SupersessionFixture::new(1, 0x100)
        .geometry(0, 0, 100, 100)
        .pending();
    assert!(present_supersession_covers(&successor, &predecessor));
}

#[test]
fn coverage_sliver_predecessor_under_full_successor_is_covered() {
    // Xorg semantics (verification item (d)): a full-frame successor
    // scraps a sliver predecessor — the successor gate cares only
    // about ITS OWN update region, not the predecessor's.
    let successor = SupersessionFixture::new(2, 0x100)
        .geometry(0, 0, 100, 100)
        .pending();
    let predecessor = SupersessionFixture::new(1, 0x100)
        .geometry(0, 0, 100, 100)
        .update_rects(Some(vec![xfixes::RegionRect {
            x: 10,
            y: 10,
            width: 5,
            height: 5,
        }]))
        .pending();
    assert!(
        present_supersession_covers(&successor, &predecessor),
        "a full-frame successor covers a thin sliver predecessor"
    );
}

#[test]
fn coverage_partial_region_successor_never_scraps_regardless_of_coverage() {
    // Xorg gate (present_scmd.c:802), amended (Task 13, spec
    // §"Amendment 2026-08-01"): a successor carrying a PARTIAL update
    // region — one that does not clear `successor_presents_full_extent`
    // — never scraps, even when its rects would geometrically cover
    // the predecessor. This is what protects marco/picom's
    // drag-sliver presents from scrapping each other. (NOT a
    // full-extent fixture: the successor's rect is a genuine sliver
    // of its 100x100 source, distinct from the full-extent case
    // covered by `coverage_single_rect_full_extent_region_scraps_like_none`.)
    let successor = SupersessionFixture::new(2, 0x100)
        .geometry(0, 0, 100, 100)
        .update_rects(Some(vec![xfixes::RegionRect {
            x: 10,
            y: 10,
            width: 5,
            height: 5,
        }]))
        .pending();
    // Predecessor's footprint sits exactly inside the successor's
    // rect — geometrically "coverable" — yet the gate still declines
    // because the successor's region isn't full-extent.
    let predecessor = SupersessionFixture::new(1, 0x100)
        .geometry(10, 10, 5, 5)
        .pending();
    assert!(!present_supersession_covers(&successor, &predecessor));
}

#[test]
fn coverage_negative_offsets_fitting_is_covered() {
    // Offsets are i16 and may be negative (a window partially
    // off-screen) — the arithmetic must not saturate.
    let successor = SupersessionFixture::new(2, 0x100)
        .geometry(-50, -50, 200, 200)
        .pending();
    let predecessor = SupersessionFixture::new(1, 0x100)
        .geometry(-40, -40, 50, 50)
        .pending();
    assert!(present_supersession_covers(&successor, &predecessor));
}

#[test]
fn coverage_negative_offsets_not_fitting_is_not_covered() {
    let successor = SupersessionFixture::new(2, 0x100)
        .geometry(-50, -50, 100, 100)
        .pending();
    // Predecessor's left edge (-60) is outside the successor's rect
    // (starts at -50).
    let predecessor = SupersessionFixture::new(1, 0x100)
        .geometry(-60, -50, 20, 20)
        .pending();
    assert!(!present_supersession_covers(&successor, &predecessor));
}

#[test]
fn coverage_full_frame_successor_from_smaller_resize_source_does_not_cover_larger_predecessor() {
    // Mid-resize: Mesa reallocated a smaller swapchain pixmap for the
    // successor, but a larger predecessor from before the resize is
    // still parked. The successor's full-extent rect is
    // SOURCE-pixmap-bounded, so it must NOT cover a predecessor
    // extending beyond it — yserver's strictly conservative extra on
    // top of Xorg's geometry-blind scrap.
    let successor = SupersessionFixture::new(2, 0x100)
        .geometry(0, 0, 50, 50)
        .pending();
    let predecessor = SupersessionFixture::new(1, 0x100)
        .geometry(0, 0, 100, 100)
        .pending();
    assert!(!present_supersession_covers(&successor, &predecessor));
}

#[test]
fn coverage_zero_pixel_successor_never_scraps() {
    // The documented zero-pixel present: `update_rects = Some(empty)`
    // still satisfies the successor gate's `is_some()` check, so it
    // never scraps — regardless of how trivially "coverable" the
    // predecessor is.
    let successor = SupersessionFixture::new(2, 0x100)
        .geometry(0, 0, 100, 100)
        .update_rects(Some(Vec::new()))
        .pending();
    let predecessor = SupersessionFixture::new(1, 0x100)
        .geometry(0, 0, 1, 1)
        .pending();
    assert!(!present_supersession_covers(&successor, &predecessor));
}

#[test]
fn coverage_zero_pixel_predecessor_is_trivially_covered() {
    // A predecessor with `Some(empty)` has no content, so it is
    // covered by any (gate-passing) successor — falls out of
    // `.all()` on an empty iterator.
    let successor = SupersessionFixture::new(2, 0x100)
        .geometry(0, 0, 100, 100)
        .pending();
    let predecessor = SupersessionFixture::new(1, 0x100)
        .geometry(0, 0, 100, 100)
        .update_rects(Some(Vec::new()))
        .pending();
    assert!(present_supersession_covers(&successor, &predecessor));
}

#[test]
fn coverage_near_i16_max_rect_offset_saturates_like_the_real_copy() {
    // Review fix: the predicate must judge coverage at exactly the
    // position the real copy lands, not at an unsaturated `i32` sum.
    // Predecessor `x_off=32000` with rect `r.x=1000` sums to an
    // unsaturated `i32` 33000, but `execute_present_pixmap_copy`
    // computes the dest x as `x_off.saturating_add(rect.x)` in `i16`,
    // which clamps to `i16::MAX` (32767) — the real copy never draws
    // past that. The successor's extent below ends exactly at 32767
    // (`x_off` is itself `i16`-bounded, so its extent can only reach
    // i16::MAX from below, never past it), so the saturated dest
    // (32767 + rect width 100 = 32867) overruns the successor's real
    // right edge by exactly 100px and must NOT be judged covered — a
    // covered-but-not-actually-copied verdict here would lose that
    // sliver.
    let successor = SupersessionFixture::new(2, 0x100)
        .geometry(32700, 0, 67, 100) // extent x in [32700, 32767)
        .pending();
    let predecessor = SupersessionFixture::new(1, 0x100)
        .geometry(32000, 0, 1000, 1000)
        .update_rects(Some(vec![xfixes::RegionRect {
            x: 1000,
            y: 0,
            width: 100,
            height: 100,
        }]))
        .pending();
    assert!(
        !present_supersession_covers(&successor, &predecessor),
        "the real copy saturates the dest x to i16::MAX = 32767, and \
             32767 + rect width 100 = 32867 overruns the successor's real \
             right edge (32767) by 100px — must not be judged covered"
    );
}

// ---------------- Task 13: successor-gate relaxation (spec
// §"Amendment 2026-08-01 — successor-gate relaxation") ----------------

#[test]
fn coverage_single_rect_full_extent_region_scraps_like_none() {
    // The NVIDIA-WSI shape from the CS2 capture: a single rect that
    // exactly equals the source extent. Must scrap exactly like
    // `update_rects == None`.
    let successor = SupersessionFixture::new(2, 0x100)
        .geometry(0, 0, 100, 100)
        .update_rects(Some(vec![xfixes::RegionRect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        }]))
        .pending();
    let predecessor = SupersessionFixture::new(1, 0x100)
        .geometry(0, 0, 50, 50)
        .pending();
    assert!(
        present_supersession_covers(&successor, &predecessor),
        "a single-rect full-extent update region scraps exactly like None"
    );
}

#[test]
fn coverage_over_large_rect_passes() {
    // A rect with negative origin and an extent past the source
    // bounds still provably contains the full extent.
    let successor = SupersessionFixture::new(2, 0x100)
        .geometry(0, 0, 100, 100)
        .update_rects(Some(vec![xfixes::RegionRect {
            x: -10,
            y: -10,
            width: 300,
            height: 300,
        }]))
        .pending();
    let predecessor = SupersessionFixture::new(1, 0x100)
        .geometry(0, 0, 100, 100)
        .pending();
    assert!(present_supersession_covers(&successor, &predecessor));
}

#[test]
fn coverage_superset_region_passes_via_containing_rect() {
    // `rects = [full-extent rect, extra sliver]` passes via the
    // containing rect — the executed copy writes the union, a
    // superset of the extent (spec amendment's "superset region"
    // paragraph).
    let successor = SupersessionFixture::new(2, 0x100)
        .geometry(0, 0, 100, 100)
        .update_rects(Some(vec![
            xfixes::RegionRect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
            xfixes::RegionRect {
                x: 150,
                y: 150,
                width: 10,
                height: 10,
            },
        ]))
        .pending();
    let predecessor = SupersessionFixture::new(1, 0x100)
        .geometry(0, 0, 100, 100)
        .pending();
    assert!(present_supersession_covers(&successor, &predecessor));
}

#[test]
fn coverage_multirect_union_but_no_single_rect_covers_declines() {
    // Two rects whose UNION covers the full extent, but neither rect
    // alone does — union coverage is deliberately not computed, so
    // this declines (the documented conservative bound). This also
    // bites harder on yserver than Xorg: yserver's y-band region
    // normalization doesn't re-coalesce vertically/horizontally
    // adjacent identical bands.
    let successor = SupersessionFixture::new(2, 0x100)
        .geometry(0, 0, 100, 100)
        .update_rects(Some(vec![
            xfixes::RegionRect {
                x: 0,
                y: 0,
                width: 50,
                height: 100,
            },
            xfixes::RegionRect {
                x: 50,
                y: 0,
                width: 50,
                height: 100,
            },
        ]))
        .pending();
    let predecessor = SupersessionFixture::new(1, 0x100)
        .geometry(0, 0, 10, 10)
        .pending();
    assert!(!present_supersession_covers(&successor, &predecessor));
}

#[test]
fn coverage_marco_style_multirect_sliver_declines() {
    // A realistic marco/picom drag-update shape: two thin horizontal
    // strips (top + bottom), whose union does NOT even cover the
    // full extent. Never scraps.
    let successor = SupersessionFixture::new(2, 0x100)
        .geometry(0, 0, 100, 100)
        .update_rects(Some(vec![
            xfixes::RegionRect {
                x: 0,
                y: 0,
                width: 100,
                height: 10,
            },
            xfixes::RegionRect {
                x: 0,
                y: 90,
                width: 100,
                height: 10,
            },
        ]))
        .pending();
    let predecessor = SupersessionFixture::new(1, 0x100)
        .geometry(0, 0, 100, 100)
        .pending();
    assert!(!present_supersession_covers(&successor, &predecessor));
}

#[test]
fn coverage_region_full_extent_one_axis_only_declines() {
    // Padded / mid-resize shape: the rect is full-extent in width but
    // not in height, on a source taller than the rect.
    let successor = SupersessionFixture::new(2, 0x100)
        .geometry(0, 0, 100, 100)
        .update_rects(Some(vec![xfixes::RegionRect {
            x: 0,
            y: 0,
            width: 100,
            height: 50,
        }]))
        .pending();
    let predecessor = SupersessionFixture::new(1, 0x100)
        .geometry(0, 0, 10, 10)
        .pending();
    assert!(!present_supersession_covers(&successor, &predecessor));
}

#[test]
fn coverage_zero_area_rect_declines() {
    // `Some([zero-area rect])` — reachable via the
    // `CREATE_REGION_FROM_GC` path, which inserts parsed rects raw,
    // bypassing `normalize_region_rects`. A zero-area rect can never
    // satisfy both `>=` conditions.
    let successor = SupersessionFixture::new(2, 0x100)
        .geometry(0, 0, 100, 100)
        .update_rects(Some(vec![xfixes::RegionRect {
            x: 0,
            y: 0,
            width: 0,
            height: 100,
        }]))
        .pending();
    let predecessor = SupersessionFixture::new(1, 0x100)
        .geometry(0, 0, 1, 1)
        .pending();
    assert!(!present_supersession_covers(&successor, &predecessor));
}

#[test]
fn supersede_single_rect_full_extent_region_scraps_like_none() {
    // End-to-end: `supersede_covered_pending_presents` scraps a
    // covered victim behind a successor carrying the NVIDIA-WSI
    // full-extent single-rect shape.
    const WINDOW: u32 = 0x0001_0009;
    const VICTIM_ID: u64 = 90;
    const SUCCESSOR_ID: u64 = 91;
    const TARGET: u64 = 500;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();

    let victim = SupersessionFixture::new(VICTIM_ID, WINDOW)
        .eff(Some(TARGET))
        .geometry(0, 0, 50, 50)
        .entry();
    state.present_pending_exec.insert(VICTIM_ID, victim);

    let successor = SupersessionFixture::new(SUCCESSOR_ID, WINDOW)
        .eff(Some(TARGET))
        .geometry(0, 0, 100, 100)
        .update_rects(Some(vec![xfixes::RegionRect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        }]))
        .update(1)
        .pending();

    supersede_covered_pending_presents(&mut state, &mut backend, &successor);

    assert!(
        state.present_pending_exec.is_empty(),
        "a full-extent single-rect successor must scrap a covered victim"
    );
}

// ---------------- Task 13: damage-arm fix (spec §"Amendment
// 2026-08-01" — damage-arm dependency) ----------------

#[test]
fn damage_full_extent_region_present_with_offset_translates_position() {
    // Failing test first (plan Task 13 Step 3): a full-extent-region
    // present with `x_off`/`y_off != 0` accumulates damage at the
    // TRANSLATED position, matching the copy arm's
    // `x_off.saturating_add(rect.x)`.
    use crate::{backend::recording::RecordedCall, server::DamageObject};

    const WINDOW: u32 = 0x0002_0001;
    const DAMAGE_XID: u32 = 0x0002_0002;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();

    state.damage_objects.insert(
        DAMAGE_XID,
        DamageObject {
            owner: ClientId(1),
            drawable: ResourceId(WINDOW),
            level: 3,
            rects: Vec::new(),
            pending_notify_fired: false,
            last_reported_geometry: None,
        },
    );

    let pending = SupersessionFixture::new(1, WINDOW)
        .geometry(20, 30, 100, 100)
        .update_rects(Some(vec![xfixes::RegionRect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        }]))
        .update(1)
        .pending();

    execute_present_pixmap_copy(&mut state, &mut backend, pending)
        .expect("copy succeeds against RecordingBackend");

    assert!(
        backend.calls().iter().any(|call| matches!(
            call,
            RecordedCall::CopyArea {
                src_x: 0,
                src_y: 0,
                dst_x: 20,
                dst_y: 30,
                width: 100,
                height: 100,
                ..
            }
        )),
        "the copy arm translates by x_off/y_off: {:?}",
        backend.calls()
    );
    let damage = state
        .damage_objects
        .get(&DAMAGE_XID)
        .expect("damage object");
    assert_eq!(
        damage.rects,
        vec![xfixes::RegionRect {
            x: 20,
            y: 30,
            width: 100,
            height: 100,
        }],
        "the damage arm must translate per-rect damage by x_off/y_off \
             exactly like the copy arm"
    );
}

#[test]
fn direct_present_success_bypasses_copy_and_keeps_completion_gate() {
    use crate::backend::recording::RecordedCall;

    const WINDOW: u32 = 0x0002_0011;
    const PRESENT_ID: u64 = 0x44;
    const TARGET_MSC: u64 = 500;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    backend.present_direct_result = true;
    let pending = SupersessionFixture::new(PRESENT_ID, WINDOW)
        .eff(Some(TARGET_MSC))
        .geometry(0, 0, 100, 100)
        .pending();

    execute_present_pixmap_copy(&mut state, &mut backend, pending)
        .expect("direct ownership succeeds");

    assert_eq!(backend.present_direct_candidates.len(), 1);
    assert!(
        backend
            .calls()
            .iter()
            .all(|call| !matches!(call, RecordedCall::CopyArea { .. })),
        "M2b success must not record the normal source-to-COW Copy"
    );
    let gate = state
        .present_complete_gate
        .get(&PRESENT_ID)
        .expect("direct completion gate installed before ownership handoff");
    assert_eq!(gate.effective_target_msc, TARGET_MSC);
}

#[test]
fn damage_unresolvable_region_with_update_flag_accumulates_full_extent() {
    // Failing test first (plan Task 13 Step 3): an `update != 0` +
    // unresolvable-region (`update_rects == None`) present
    // accumulates FULL-EXTENT damage, not nothing — the branch is
    // re-keyed off `update_rects.is_none()`, not the raw `update`
    // xid, so this can no longer silently drop all damage.
    use crate::server::DamageObject;

    const WINDOW: u32 = 0x0002_0003;
    const DAMAGE_XID: u32 = 0x0002_0004;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();

    state.damage_objects.insert(
        DAMAGE_XID,
        DamageObject {
            owner: ClientId(1),
            drawable: ResourceId(WINDOW),
            level: 3,
            rects: Vec::new(),
            pending_notify_fired: false,
            last_reported_geometry: None,
        },
    );

    let pending = SupersessionFixture::new(1, WINDOW)
        .geometry(5, 7, 60, 40)
        .update_rects(None)
        .update(1)
        .pending();

    execute_present_pixmap_copy(&mut state, &mut backend, pending)
        .expect("copy succeeds against RecordingBackend");

    let damage = state
        .damage_objects
        .get(&DAMAGE_XID)
        .expect("damage object");
    assert_eq!(
        damage.rects,
        vec![xfixes::RegionRect {
            x: 5,
            y: 7,
            width: 60,
            height: 40,
        }],
        "update != 0 with an unresolvable region must still damage \
             full-extent, not nothing"
    );
}

// ---------------- Step 2: supersede_covered_pending_presents ----------------

/// Seed one `Present` event selection for `window` with the given
/// mask, draining the connection setup traffic first. Tests that
/// decode the wire with `complete_notify_modes` (fixed 40-byte
/// chunks) pass `EVENT_MASK_COMPLETE_NOTIFY` alone — mixing in
/// `EVENT_MASK_IDLE_NOTIFY` would interleave 32-byte `IdleNotify`
/// events into that fixed-width chunking (Task 6's
/// `due_skip_is_held_back_behind_a_smaller_id_undrained_gate_entry`
/// makes the same choice for the same reason).
fn seed_present_selection(
    state: &mut ServerState,
    peer: &mut UnixStream,
    eid: u32,
    window: u32,
    mask: u32,
) {
    state.present_event_selections.insert(
        eid,
        crate::server::PresentEventSelection {
            owner: ClientId(1),
            window: ResourceId(window),
            event_mask: mask,
        },
    );
    let _ = read_all_available(peer);
}

#[test]
fn supersede_scraps_covered_same_window_same_target_entry() {
    const WINDOW: u32 = 0x0001_0001;
    const PRESENT_EID: u32 = 0x0020_1001;
    const VICTIM_ID: u64 = 10;
    const SUCCESSOR_ID: u64 = 11;
    const TARGET: u64 = 500;
    const IDLE_FENCE: u32 = 0x0030_1001;
    const WAIT_ID: u64 = 777;
    const PIN_ID: u64 = 1;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    seed_present_selection(
        &mut state,
        &mut peer,
        PRESENT_EID,
        WINDOW,
        yserver_protocol::x11::present::EVENT_MASK_COMPLETE_NOTIFY
            | yserver_protocol::x11::present::EVENT_MASK_IDLE_NOTIFY,
    );
    let mut backend = RecordingBackend::new();

    state.sync_fences.insert(
        IDLE_FENCE,
        crate::server::SyncFence {
            owner: ClientId(1),
            triggered: false,
        },
    );

    let mut victim = SupersessionFixture::new(VICTIM_ID, WINDOW)
        .eff(Some(TARGET))
        .geometry(0, 0, 50, 50)
        .idle_fence(IDLE_FENCE)
        .entry();
    victim.wait_id = Some(WAIT_ID);
    victim.pin = Some(PIN_ID);
    state.present_pending_exec.insert(VICTIM_ID, victim);
    state.present_wait_to_id.insert(WAIT_ID, VICTIM_ID);

    let successor = SupersessionFixture::new(SUCCESSOR_ID, WINDOW)
        .eff(Some(TARGET))
        .geometry(0, 0, 100, 100)
        .pending();

    supersede_covered_pending_presents(&mut state, &mut backend, &successor);

    assert!(
        state.present_pending_exec.is_empty(),
        "the covered victim must be removed from the store"
    );
    assert!(
        state.present_wait_to_id.is_empty(),
        "the side-map row must be dropped with the entry"
    );
    assert_eq!(
        backend.finished_present_source_waits,
        vec![WAIT_ID],
        "the armed source wait must be cancelled"
    );
    assert_eq!(
        backend.released_present_sources,
        vec![PIN_ID],
        "the entry pin must be released"
    );
    assert_eq!(
        backend.triggered_dri3_fences,
        vec![IDLE_FENCE],
        "the idle fence must be triggered by XID, immediately"
    );
    assert!(
        state.sync_fences[&IDLE_FENCE].triggered,
        "the X11 fence mirror must be set at scrap time"
    );
    assert_eq!(
        state.present_pending_complete.len(),
        1,
        "a Skip completion must be parked for ordered delivery"
    );
    let parked = &state.present_pending_complete[0];
    assert_eq!(parked.event.present_id, VICTIM_ID);
    assert_eq!(parked.effective_target_msc, TARGET);
    assert_eq!(
        parked.mode,
        yserver_protocol::x11::present::COMPLETE_MODE_SKIP
    );
    assert!(!parked.emit_idle, "IdleNotify already fired at scrap");

    // Review fix (5a): actually decode the IdleNotify wire bytes
    // (layout per `encode_idle_notify`), not just count them —
    // pins event type, eid, window, pixmap xid, and fence xid.
    let idle_bytes = read_all_available(&mut peer);
    assert_eq!(
        idle_bytes.len(),
        32,
        "exactly one IdleNotify (32 bytes) must be delivered on the wire right \
             now, at scrap time — no CompleteNotify yet"
    );
    assert_eq!(idle_bytes[0], 35, "GenericEvent");
    assert_eq!(idle_bytes[1], 145, "PRESENT major opcode");
    assert_eq!(
        idle_bytes[8],
        yserver_protocol::x11::present::EVENT_IDLE_NOTIFY,
        "evtype must be IdleNotify"
    );
    assert_eq!(
        u32::from_le_bytes(idle_bytes[12..16].try_into().unwrap()),
        PRESENT_EID,
        "eid must be the selection that requested IdleNotify"
    );
    assert_eq!(
        u32::from_le_bytes(idle_bytes[16..20].try_into().unwrap()),
        WINDOW,
        "window xid"
    );
    assert_eq!(
        u32::from_le_bytes(idle_bytes[24..28].try_into().unwrap()),
        0x1,
        "pixmap xid — the victim's own client-visible pixmap (SupersessionFixture \
             hardcodes 0x1)"
    );
    assert_eq!(
        u32::from_le_bytes(idle_bytes[28..32].try_into().unwrap()),
        IDLE_FENCE,
        "fence xid"
    );
}

#[test]
fn supersede_note_present_skip_once_per_scrapped_victim() {
    // Task 10 telemetry: `Backend::note_present_skip` must fire exactly
    // once per victim actually scrapped by supersession — not once per
    // call, not for a survivor, and not double-counted.
    const WINDOW: u32 = 0x0001_1002;
    const VICTIM_A: u64 = 200;
    const VICTIM_B: u64 = 201;
    const SUCCESSOR_ID: u64 = 202;
    const TARGET: u64 = 500;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();

    let victim_a = SupersessionFixture::new(VICTIM_A, WINDOW)
        .eff(Some(TARGET))
        .geometry(0, 0, 10, 10)
        .entry();
    let victim_b = SupersessionFixture::new(VICTIM_B, WINDOW)
        .eff(Some(TARGET))
        .geometry(20, 20, 10, 10)
        .entry();
    state.present_pending_exec.insert(VICTIM_A, victim_a);
    state.present_pending_exec.insert(VICTIM_B, victim_b);

    let successor = SupersessionFixture::new(SUCCESSOR_ID, WINDOW)
        .eff(Some(TARGET))
        .geometry(0, 0, 100, 100)
        .pending();

    supersede_covered_pending_presents(&mut state, &mut backend, &successor);

    assert!(
        state.present_pending_exec.is_empty(),
        "both covered victims must be scrapped"
    );
    assert_eq!(
        backend.present_skip_count, 2,
        "note_present_skip must fire exactly once per scrapped victim"
    );
}

#[test]
fn supersede_leaves_entry_with_different_effective_target_msc() {
    const WINDOW: u32 = 0x0001_0002;
    const VICTIM_ID: u64 = 20;
    const SUCCESSOR_ID: u64 = 21;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();

    let victim = SupersessionFixture::new(VICTIM_ID, WINDOW)
        .eff(Some(500))
        .geometry(0, 0, 50, 50)
        .entry();
    state.present_pending_exec.insert(VICTIM_ID, victim);

    let successor = SupersessionFixture::new(SUCCESSOR_ID, WINDOW)
        .eff(Some(600)) // different target
        .geometry(0, 0, 100, 100)
        .pending();

    supersede_covered_pending_presents(&mut state, &mut backend, &successor);

    assert_eq!(
        state.present_pending_exec.len(),
        1,
        "an entry targeting a different effective MSC must survive"
    );
    assert!(state.present_pending_complete.is_empty());
}

#[test]
fn supersede_leaves_entry_in_different_window() {
    const WINDOW_A: u32 = 0x0001_0003;
    const WINDOW_B: u32 = 0x0001_0004;
    const VICTIM_ID: u64 = 30;
    const SUCCESSOR_ID: u64 = 31;
    const TARGET: u64 = 500;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();

    let victim = SupersessionFixture::new(VICTIM_ID, WINDOW_A)
        .eff(Some(TARGET))
        .geometry(0, 0, 50, 50)
        .entry();
    state.present_pending_exec.insert(VICTIM_ID, victim);

    let successor = SupersessionFixture::new(SUCCESSOR_ID, WINDOW_B)
        .eff(Some(TARGET))
        .geometry(0, 0, 100, 100)
        .pending();

    supersede_covered_pending_presents(&mut state, &mut backend, &successor);

    assert_eq!(
        state.present_pending_exec.len(),
        1,
        "an entry in a different window must survive"
    );
    assert!(state.present_pending_complete.is_empty());
}

#[test]
fn supersede_partial_region_successor_with_update_rects_never_scraps() {
    // Task 13 refit (spec §"Amendment 2026-08-01"): the fixture's
    // successor rect is a genuine sliver of its 100x100 source, not
    // an accidentally-full-extent one — post-amendment a full-extent
    // single rect WOULD clear the gate (see
    // `coverage_single_rect_full_extent_region_scraps_like_none`),
    // so this test's coverage of "a partial-region successor never
    // scraps" depends on the rect staying genuinely partial.
    const WINDOW: u32 = 0x0001_0005;
    const VICTIM_ID: u64 = 40;
    const SUCCESSOR_ID: u64 = 41;
    const TARGET: u64 = 500;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();

    // Victim's footprint sits exactly inside the successor's rect —
    // geometrically "coverable" — yet the gate still declines because
    // the successor's region isn't full-extent.
    let victim = SupersessionFixture::new(VICTIM_ID, WINDOW)
        .eff(Some(TARGET))
        .geometry(10, 10, 5, 5)
        .entry();
    state.present_pending_exec.insert(VICTIM_ID, victim);

    let successor = SupersessionFixture::new(SUCCESSOR_ID, WINDOW)
        .eff(Some(TARGET))
        .geometry(0, 0, 100, 100)
        .update_rects(Some(vec![xfixes::RegionRect {
            x: 10,
            y: 10,
            width: 5,
            height: 5,
        }]))
        .pending();

    supersede_covered_pending_presents(&mut state, &mut backend, &successor);

    assert_eq!(
        state.present_pending_exec.len(),
        1,
        "a successor carrying a partial update region must never scrap"
    );
    assert!(state.present_pending_complete.is_empty());
}

#[test]
fn supersede_successor_with_no_effective_target_never_scraps() {
    // With `eff = None`, equivalence with any pending request is unknown.
    const WINDOW: u32 = 0x0001_0006;
    const VICTIM_ID: u64 = 50;
    const SUCCESSOR_ID: u64 = 51;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();

    let victim = SupersessionFixture::new(VICTIM_ID, WINDOW)
        .eff(Some(500))
        .geometry(0, 0, 50, 50)
        .entry();
    state.present_pending_exec.insert(VICTIM_ID, victim);

    let successor = SupersessionFixture::new(SUCCESSOR_ID, WINDOW)
        .eff(None) // no effective target: no shared Some target to scrap on
        .geometry(0, 0, 100, 100)
        .pending();

    supersede_covered_pending_presents(&mut state, &mut backend, &successor);

    assert_eq!(
        state.present_pending_exec.len(),
        1,
        "a successor with no effective target never scraps"
    );
    assert!(state.present_pending_complete.is_empty());
}

#[test]
fn async_requests_with_same_effective_target_supersede() {
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let mut pred_entry = present_pending_entry_with(1, 0x00e0_3001, 0x00e0_3002, Some(500), true);
    pred_entry.pending.masked_options = crate::present_scheduler::PRESENT_ALL_ASYNC_OPTIONS;
    let pred = pred_entry.pending.clone();
    state.present_pending_exec.insert(1, pred_entry);
    let succ = crate::server::PendingPresentPixmap {
        present_id: 2,
        effective_target_msc: Some(500),
        masked_options: crate::present_scheduler::PRESENT_ALL_ASYNC_OPTIONS,
        ..pred
    };
    supersede_covered_pending_presents(&mut state, &mut backend, &succ);
    assert!(
        !state.present_pending_exec.contains_key(&1),
        "async requests with the same known effective target must supersede"
    );
    assert_eq!(backend.present_skip_count, 1);
    assert_eq!(
        state.present_pending_complete.len(),
        1,
        "Skip parked for ordered delivery"
    );
}

#[test]
fn async_requests_with_unknown_effective_targets_do_not_supersede() {
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let mut pred_entry = present_pending_entry_with(1, 0x00e0_3001, 0x00e0_3002, None, true);
    pred_entry.pending.masked_options = crate::present_scheduler::PRESENT_ALL_ASYNC_OPTIONS;
    let pred = pred_entry.pending.clone();
    state.present_pending_exec.insert(1, pred_entry);
    let succ = crate::server::PendingPresentPixmap {
        present_id: 2,
        effective_target_msc: None,
        masked_options: crate::present_scheduler::PRESENT_ALL_ASYNC_OPTIONS,
        ..pred
    };
    supersede_covered_pending_presents(&mut state, &mut backend, &succ);
    assert!(
        state.present_pending_exec.contains_key(&1),
        "None does not establish target equivalence, even for two async requests"
    );
    assert_eq!(backend.present_skip_count, 0);
}

#[test]
fn supersede_pixmap_synced_victim_releases_via_syncobj() {
    const WINDOW: u32 = 0x0001_0007;
    const VICTIM_ID: u64 = 60;
    const SUCCESSOR_ID: u64 = 61;
    const TARGET: u64 = 500;
    const RELEASE_SYNCOBJ: u32 = 0x0030_1007;
    const RELEASE_VALUE: u64 = 42;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();

    let release = backend.seed_dri3_syncobj_for_test(RELEASE_SYNCOBJ, ClientId(1));
    let mut victim = SupersessionFixture::new(VICTIM_ID, WINDOW)
        .eff(Some(TARGET))
        .geometry(0, 0, 50, 50)
        .synced_release(RELEASE_SYNCOBJ, RELEASE_VALUE)
        .entry();
    victim.pending.wake = crate::backend::PresentWake::PixmapSynced {
        release,
        release_syncobj: RELEASE_SYNCOBJ,
        release_value: RELEASE_VALUE,
    };
    state.present_pending_exec.insert(VICTIM_ID, victim);

    let successor = SupersessionFixture::new(SUCCESSOR_ID, WINDOW)
        .eff(Some(TARGET))
        .geometry(0, 0, 100, 100)
        .pending();

    supersede_covered_pending_presents(&mut state, &mut backend, &successor);

    assert!(state.present_pending_exec.is_empty());
    assert_eq!(
        *backend.signalled_dri3_syncobjs.lock().unwrap(),
        vec![(RELEASE_SYNCOBJ, RELEASE_VALUE)],
        "a PixmapSynced victim releases via dri3_signal_syncobj, not dri3_trigger_fence"
    );
    assert!(backend.triggered_dri3_fences.is_empty());
    assert_eq!(state.present_pending_complete.len(), 1);
}

#[test]
fn arm_before_scrap_syncobj_failure_leaves_covered_victim_untouched() {
    // Regression for the adversarial-review blocker: the successor's
    // own `arm_present_syncobj_wait` is client-reachably fallible
    // (e.g. an unknown acquire syncobj). If supersession scrapped
    // covered victims BEFORE that arm, a failed arm would unwind via
    // `?` with the victim already destroyed and nothing committed to
    // replace it — leaving the window with no frame at the target
    // MSC where Xorg would still have shown the victim. This drives
    // the real `arm_present_pixmap_synced_source_then_supersede`
    // helper — the exact arm+scrap sequence the PixmapSynced handler
    // calls — rather than invoking
    // `supersede_covered_pending_presents` directly, so it actually
    // exercises production ordering rather than hand-waving it.
    const WINDOW: u32 = 0x0001_0008;
    const VICTIM_ID: u64 = 62;
    const SUCCESSOR_ID: u64 = 63;
    const TARGET: u64 = 500;
    const IDLE_FENCE: u32 = 0x0030_1008;
    const WAIT_ID: u64 = 778;
    const PIN_ID: u64 = 2;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    backend.arm_present_syncobj_wait_result = Some(std::io::ErrorKind::InvalidInput);

    state.sync_fences.insert(
        IDLE_FENCE,
        crate::server::SyncFence {
            owner: ClientId(1),
            triggered: false,
        },
    );

    let mut victim = SupersessionFixture::new(VICTIM_ID, WINDOW)
        .eff(Some(TARGET))
        .geometry(0, 0, 50, 50)
        .idle_fence(IDLE_FENCE)
        .entry();
    victim.wait_id = Some(WAIT_ID);
    victim.pin = Some(PIN_ID);
    state.present_pending_exec.insert(VICTIM_ID, victim);
    state.present_wait_to_id.insert(WAIT_ID, VICTIM_ID);

    let successor = SupersessionFixture::new(SUCCESSOR_ID, WINDOW)
        .eff(Some(TARGET))
        .geometry(0, 0, 100, 100)
        .pending();

    let result =
        arm_present_pixmap_synced_source_then_supersede(&mut state, &mut backend, 0, 0, &successor);

    assert!(
        result.is_err(),
        "the canned arm failure must propagate as Err"
    );
    assert_eq!(
        state.present_pending_exec.len(),
        1,
        "the covered victim must still be in present_pending_exec — scrap must \
             not run ahead of a failed arm"
    );
    assert!(
        state.present_pending_exec.contains_key(&VICTIM_ID),
        "specifically the victim, untouched"
    );
    assert!(
        state.present_wait_to_id.contains_key(&WAIT_ID),
        "the victim's wait side-map row must survive too"
    );
    assert!(
        backend.finished_present_source_waits.is_empty(),
        "the victim's wait must not be cancelled"
    );
    assert!(
        backend.released_present_sources.is_empty(),
        "the victim's pin must not be released"
    );
    assert!(
        backend.triggered_dri3_fences.is_empty(),
        "the victim's idle fence must NOT be triggered"
    );
    assert!(
        !state.sync_fences[&IDLE_FENCE].triggered,
        "the X11 fence mirror must NOT be set — the victim's fence never fired"
    );
    assert!(
        state.present_pending_complete.is_empty(),
        "no Skip completion may be parked for a victim that was never scrapped"
    );
}

// ---------------- Step 3: copy-failure reroute ----------------

#[test]
fn failing_copy_at_arrival_parks_ordered_copy_completion_and_preserves_window_order() {
    const WINDOW: u32 = 0x0001_0101;
    const PRESENT_EID: u32 = 0x0020_1101;
    const BLOCKER_ID: u64 = 70;
    const FAILING_ID: u64 = 71;
    const TARGET: u64 = 500;
    const IDLE_FENCE: u32 = 0x0030_1101;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    // Review fix (5b): also select IdleNotify, so this test can pin
    // that exactly one fires — at failure time, with the right fence
    // — and none at delivery.
    seed_present_selection(
        &mut state,
        &mut peer,
        PRESENT_EID,
        WINDOW,
        yserver_protocol::x11::present::EVENT_MASK_COMPLETE_NOTIFY
            | yserver_protocol::x11::present::EVENT_MASK_IDLE_NOTIFY,
    );
    let mut backend = RecordingBackend::new();
    backend.fail_copy_area = true;

    state.sync_fences.insert(
        IDLE_FENCE,
        crate::server::SyncFence {
            owner: ClientId(1),
            triggered: false,
        },
    );

    // An earlier same-window present is still unresolved (msc-parked)
    // — the failing present's completion must be held back behind it.
    state
        .present_pending_exec
        .insert(BLOCKER_ID, stub_pending_present_entry(WINDOW, BLOCKER_ID));

    let failing = SupersessionFixture::new(FAILING_ID, WINDOW)
        .eff(Some(TARGET))
        .idle_fence(IDLE_FENCE)
        .pending();

    let ok = execute_present_pixmap_copy_or_reroute(&mut state, &mut backend, failing);
    assert!(!ok, "a failing copy_area must report failure");

    assert!(
        backend
            .calls()
            .iter()
            .any(|c| matches!(c, crate::backend::recording::RecordedCall::CopyArea { .. })),
        "the copy must actually be attempted"
    );
    assert!(
        state.present_complete_gate.is_empty(),
        "a failed copy runs before the gate insert — no gate row must exist"
    );
    assert_eq!(
        backend.triggered_dri3_fences,
        vec![IDLE_FENCE],
        "the buffer must be released by XID immediately, like scrap"
    );
    assert!(state.sync_fences[&IDLE_FENCE].triggered);

    // Exactly one IdleNotify must be on the wire NOW, at failure time
    // — the reroute's by-XID release fires it immediately, same as
    // scrap (Fix 5a's decode pattern).
    let idle_bytes = read_all_available(&mut peer);
    assert_eq!(
        idle_bytes.len(),
        32,
        "exactly one IdleNotify (32 bytes) must fire at failure time"
    );
    assert_eq!(idle_bytes[0], 35, "GenericEvent");
    assert_eq!(idle_bytes[1], 145, "PRESENT major opcode");
    assert_eq!(
        idle_bytes[8],
        yserver_protocol::x11::present::EVENT_IDLE_NOTIFY,
        "evtype must be IdleNotify"
    );
    assert_eq!(
        u32::from_le_bytes(idle_bytes[28..32].try_into().unwrap()),
        IDLE_FENCE,
        "fence xid"
    );

    assert_eq!(state.present_pending_complete.len(), 1);
    let parked = &state.present_pending_complete[0];
    assert_eq!(parked.event.present_id, FAILING_ID);
    assert_eq!(parked.effective_target_msc, TARGET);
    assert_eq!(
        parked.mode,
        yserver_protocol::x11::present::COMPLETE_MODE_COPY
    );
    assert!(!parked.emit_idle);

    let clock = crate::backend::PresentClockSample {
        msc: TARGET,
        ust: 0x1000,
        source: crate::backend::PresentClockSource::PageFlip,
    };
    fire_due_present_completions(&mut state, &mut backend, clock);
    assert_eq!(
        state.present_pending_complete.len(),
        1,
        "the parked Copy completion stays held back behind the still-\
             unexecuted blocker in present_pending_exec"
    );

    // The blocker resolves.
    state.present_pending_exec.remove(&BLOCKER_ID);
    fire_due_present_completions(&mut state, &mut backend, clock);
    assert!(
        state.present_pending_complete.is_empty(),
        "the rerouted Copy completion delivers once the blocker clears"
    );
    assert!(
        backend.signalled_present_wakes.is_empty(),
        "emit_idle=false must not signal the (already-released) wake a second time"
    );
    // No second IdleNotify at delivery: `complete_notify_modes` chunks
    // strictly by 40 bytes and asserts each chunk is a well-formed
    // CompleteNotify, so a stray 32-byte IdleNotify here would
    // misalign the chunking and fail loudly rather than pass
    // silently.
    let wire = read_all_available(&mut peer);
    assert_eq!(
        complete_notify_modes(&wire),
        vec![yserver_protocol::x11::present::COMPLETE_MODE_COPY]
    );
}

#[test]
fn failing_copy_without_clock_delivers_complete_notify_inline() {
    const WINDOW: u32 = 0x0001_0102;
    const PRESENT_EID: u32 = 0x0020_1102;
    const FAILING_ID: u64 = 72;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    seed_present_selection(
        &mut state,
        &mut peer,
        PRESENT_EID,
        WINDOW,
        yserver_protocol::x11::present::EVENT_MASK_COMPLETE_NOTIFY,
    );
    let mut backend = RecordingBackend::new();
    backend.fail_copy_area = true;
    seed_present_clock(&mut state, 100, 0x2000);

    let failing = SupersessionFixture::new(FAILING_ID, WINDOW)
        .eff(None) // no usable CRTC clock
        .pending();

    let ok = execute_present_pixmap_copy_or_reroute(&mut state, &mut backend, failing);
    assert!(!ok);

    assert!(
        state.present_pending_complete.is_empty(),
        "a no-clock (eff=None) failing copy must deliver inline, not park"
    );
    assert!(
        backend.signalled_present_wakes.is_empty(),
        "emit_idle=false: no wake is signalled"
    );
    let wire = read_all_available(&mut peer);
    assert_eq!(
        complete_notify_modes(&wire),
        vec![yserver_protocol::x11::present::COMPLETE_MODE_COPY],
        "the completion delivers immediately with mode Copy"
    );
}

#[test]
fn failing_copy_in_due_pass_releases_entry_pin_exactly_once() {
    const WINDOW: u32 = 0x0001_0103;
    const FAILING_ID: u64 = 73;
    const PIN_ID: u64 = 9;
    const TARGET: u64 = 500;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    backend.fail_copy_area = true;

    let mut entry = SupersessionFixture::new(FAILING_ID, WINDOW)
        .eff(Some(TARGET))
        .entry();
    entry.pin = Some(PIN_ID);
    state.present_pending_exec.insert(FAILING_ID, entry);

    execute_parked_present_ids(&mut state, &mut backend, &[FAILING_ID], "drain");

    assert!(state.present_pending_exec.is_empty());
    assert_eq!(
        backend.released_present_sources,
        vec![PIN_ID],
        "the entry pin must be released exactly once, even on copy failure"
    );
    assert!(
        backend
            .calls()
            .iter()
            .all(|c| !matches!(c, crate::backend::recording::RecordedCall::MarkDirty)),
        "mark_dirty must not fire when the copy failed"
    );
    assert_eq!(state.present_pending_complete.len(), 1);
}

// ---------------- Step 4: end-to-end vectors ----------------

/// Round-3 inversion vector, now driven by REAL scrap (Task 6's
/// version at `due_skip_is_held_back_behind_a_smaller_id_undrained_gate_entry`
/// inserted the Skip row by hand): P1 executes at arrival (no flip in
/// flight), P2 parks (a flip is in flight), P3 arrives full-frame at
/// the same effective target and scraps P2 via
/// `supersede_covered_pending_presents`. Skip(P2) must not deliver
/// before Copy(P1) even though it was parked first (at P3's arrival,
/// before P1's GPU fence retired).
#[test]
fn supersession_e2e_p1_executes_p2_parks_p3_scraps_p2_delivery_order_preserved() {
    use yserver_protocol::x11::present as x11present;

    const WINDOW: u32 = 0x0001_0201;
    const PRESENT_EID: u32 = 0x0020_1201;
    const P1_ID: u64 = 80;
    const P2_ID: u64 = 81;
    const P3_ID: u64 = 82;
    const CLOCK: u64 = 10;
    const TARGET: u64 = CLOCK + 1; // eff for target_msc=0/divisor=0/remainder=0

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    backend.present_ust_msc = (CLOCK, 0x1000); // held fixed across all three arrivals
    seed_present_clock(&mut state, CLOCK, 0x1000);

    // P1: no flip in flight -> ExecuteNow.
    backend.present_flip_in_flight = false;
    let p1 = SupersessionFixture::new(P1_ID, WINDOW)
        .eff(Some(TARGET))
        .geometry(0, 0, 100, 100)
        .pending();
    supersede_covered_pending_presents(&mut state, &mut backend, &p1);
    arrival_execute_or_park_present_pixmap(&mut state, &mut backend, P1_ID, p1);
    assert!(
        state.present_complete_gate.contains_key(&P1_ID),
        "P1 executed at arrival and inserted its completion gate"
    );

    // P2: a flip is now in flight -> Park.
    backend.present_flip_in_flight = true;
    let p2 = SupersessionFixture::new(P2_ID, WINDOW)
        .eff(Some(TARGET))
        .geometry(0, 0, 100, 100)
        .pending();
    supersede_covered_pending_presents(&mut state, &mut backend, &p2);
    arrival_execute_or_park_present_pixmap(&mut state, &mut backend, P2_ID, p2);
    assert!(state.present_pending_exec.contains_key(&P2_ID));

    // P3: full-frame, same effective target -> scraps P2, then parks
    // itself (flip still in flight).
    let p3 = SupersessionFixture::new(P3_ID, WINDOW)
        .eff(Some(TARGET))
        .geometry(0, 0, 100, 100)
        .pending();
    supersede_covered_pending_presents(&mut state, &mut backend, &p3);
    assert!(
        !state.present_pending_exec.contains_key(&P2_ID),
        "P3 scrapped P2"
    );
    arrival_execute_or_park_present_pixmap(&mut state, &mut backend, P3_ID, p3);
    assert!(state.present_pending_exec.contains_key(&P3_ID));

    assert_eq!(
        state.present_pending_complete.len(),
        1,
        "Skip(P2) is parked"
    );
    assert_eq!(state.present_pending_complete[0].event.present_id, P2_ID);

    // Wire up the completion-notify subscriber now that the store
    // setup above is done, and drain its connection setup traffic.
    let mut peer = install_client(&mut state, 1);
    state.present_event_selections.insert(
        PRESENT_EID,
        crate::server::PresentEventSelection {
            owner: ClientId(1),
            window: ResourceId(WINDOW),
            event_mask: x11present::EVENT_MASK_COMPLETE_NOTIFY,
        },
    );
    let _ = read_all_available(&mut peer);

    let clock = crate::backend::PresentClockSample {
        msc: TARGET,
        ust: 0x2000,
        source: crate::backend::PresentClockSource::PageFlip,
    };
    fire_due_present_completions(&mut state, &mut backend, clock);
    assert_eq!(
        state.present_pending_complete.len(),
        1,
        "Skip(P2) held back behind P1's still-undrained completion gate"
    );

    // P1's fence retires late: the gate empties and its completion
    // joins the queue, as run.rs's Some(gate) arm does.
    state.present_complete_gate.remove(&P1_ID);
    state
        .present_pending_complete
        .push(crate::server::PendingPresentComplete {
            event: crate::backend::CompletedPresentEvent {
                client_id: ClientId(1),
                serial: 1,
                host_xid: 0x1,
                dst_host_xid: WINDOW,
                options: 0,
                present_id: P1_ID,
                window_generation: 0,
                crtc_id: 0,
                crtc_epoch: 0,
                msc_offset: 0,
                completion_clock: None,
                wake: crate::backend::PresentWake::Pixmap { idle_fence_xid: 0 },
                completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
                emit_idle: true,
            },
            effective_target_msc: TARGET,
            mode: x11present::COMPLETE_MODE_COPY,
            emit_idle: true,
        });

    fire_due_present_completions(&mut state, &mut backend, clock);
    assert!(state.present_pending_complete.is_empty());
    assert_eq!(
        complete_notify_modes(&read_all_available(&mut peer)),
        vec![
            x11present::COMPLETE_MODE_COPY,
            x11present::COMPLETE_MODE_SKIP
        ],
        "Copy(P1) must deliver before Skip(P2) — never inverted"
    );
}

/// Uncovered survivor (spec vector (ii)): A carries a sliver update
/// region NOT covered by successor C's extent, so C's scrap declines
/// it; B is full-frame and covered, so C scraps it. A later executes
/// via the due-pass; Skip(B) must not deliver before Copy(A) even
/// though B has the larger present_id and was scrapped (queued)
/// first.
#[test]
fn supersede_declined_uncovered_survivor_holds_back_covered_scrap_skip() {
    use yserver_protocol::x11::present as x11present;

    const WINDOW: u32 = 0x0001_0202;
    const PRESENT_EID: u32 = 0x0020_1202;
    const A_ID: u64 = 90;
    const B_ID: u64 = 91;
    const C_ID: u64 = 92;
    const TARGET: u64 = 500;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    backend.present_ust_msc = (TARGET, 0x1000); // due at TARGET.

    // A: sliver update region far outside C's extent (declines).
    let a = SupersessionFixture::new(A_ID, WINDOW)
        .eff(Some(TARGET))
        .geometry(0, 0, 100, 100)
        .update_rects(Some(vec![xfixes::RegionRect {
            x: 200,
            y: 200,
            width: 5,
            height: 5,
        }]))
        .entry();
    state.present_pending_exec.insert(A_ID, a);

    // B: full-frame, covered.
    let b = SupersessionFixture::new(B_ID, WINDOW)
        .eff(Some(TARGET))
        .geometry(0, 0, 100, 100)
        .entry();
    state.present_pending_exec.insert(B_ID, b);

    let c = SupersessionFixture::new(C_ID, WINDOW)
        .eff(Some(TARGET))
        .geometry(0, 0, 100, 100)
        .pending();
    supersede_covered_pending_presents(&mut state, &mut backend, &c);

    assert!(
        state.present_pending_exec.contains_key(&A_ID),
        "A's sliver update region is not covered by C's extent — declined"
    );
    assert!(
        !state.present_pending_exec.contains_key(&B_ID),
        "B is full-frame and covered — scrapped"
    );
    assert_eq!(state.present_pending_complete.len(), 1);
    assert_eq!(state.present_pending_complete[0].event.present_id, B_ID);

    let mut peer = install_client(&mut state, 1);
    state.present_event_selections.insert(
        PRESENT_EID,
        crate::server::PresentEventSelection {
            owner: ClientId(1),
            window: ResourceId(WINDOW),
            event_mask: x11present::EVENT_MASK_COMPLETE_NOTIFY,
        },
    );
    let _ = read_all_available(&mut peer);

    drain_due_present_pending_exec(&mut state, &mut backend);
    assert!(
        state.present_pending_exec.is_empty(),
        "A executes in the due-pass"
    );
    assert!(
        state.present_complete_gate.contains_key(&A_ID),
        "A's execute_present_pixmap_copy inserted its completion gate"
    );

    let clock = crate::backend::PresentClockSample {
        msc: TARGET,
        ust: 0x1000,
        source: crate::backend::PresentClockSource::PageFlip,
    };
    fire_due_present_completions(&mut state, &mut backend, clock);
    assert_eq!(
        state.present_pending_complete.len(),
        1,
        "Skip(B) held back behind A's still-undrained completion gate"
    );

    // A's fence retires: the gate empties and its completion joins
    // the queue, as run.rs's Some(gate) arm does.
    state.present_complete_gate.remove(&A_ID);
    state
        .present_pending_complete
        .push(crate::server::PendingPresentComplete {
            event: crate::backend::CompletedPresentEvent {
                client_id: ClientId(1),
                serial: 1,
                host_xid: 0x1,
                dst_host_xid: WINDOW,
                options: 0,
                present_id: A_ID,
                window_generation: 0,
                crtc_id: 0,
                crtc_epoch: 0,
                msc_offset: 0,
                completion_clock: None,
                wake: crate::backend::PresentWake::Pixmap { idle_fence_xid: 0 },
                completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
                emit_idle: true,
            },
            effective_target_msc: TARGET,
            mode: x11present::COMPLETE_MODE_COPY,
            emit_idle: true,
        });

    fire_due_present_completions(&mut state, &mut backend, clock);
    assert!(state.present_pending_complete.is_empty());
    assert_eq!(
        complete_notify_modes(&read_all_available(&mut peer)),
        vec![
            x11present::COMPLETE_MODE_COPY,
            x11present::COMPLETE_MODE_SKIP
        ],
        "Copy(A) must deliver before Skip(B): A < B for the same window"
    );
}

/// Same-target entries whose successor declines (or never arrives)
/// all execute at their due point in arrival (`present_id`) order.
#[test]
fn uncovered_same_target_entries_execute_in_arrival_order() {
    use crate::backend::recording::RecordedCall;

    const WINDOW: u32 = 0x0001_0203;
    const A_ID: u64 = 100;
    const B_ID: u64 = 101;
    const TARGET: u64 = 500;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    backend.present_ust_msc = (TARGET, 0x1000);

    // Both carry a sliver update region, so a would-be successor
    // would decline both — but here we just prove arrival order
    // survives when nothing scraps at all (present_id insertion
    // order == BTreeMap iteration order).
    let a = SupersessionFixture::new(A_ID, WINDOW)
        .eff(Some(TARGET))
        .update_rects(Some(vec![xfixes::RegionRect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        }]))
        .entry();
    state.present_pending_exec.insert(A_ID, a);
    let b = SupersessionFixture::new(B_ID, WINDOW)
        .eff(Some(TARGET))
        .update_rects(Some(vec![xfixes::RegionRect {
            x: 20,
            y: 20,
            width: 10,
            height: 10,
        }]))
        .entry();
    state.present_pending_exec.insert(B_ID, b);

    drain_due_present_pending_exec(&mut state, &mut backend);

    let order: Vec<i16> = backend
        .calls()
        .iter()
        .filter_map(|c| match c {
            RecordedCall::CopyArea { dst_x, .. } => Some(*dst_x),
            _ => None,
        })
        .collect();
    // dst_x mirrors the rect's x (0 for A, 20 for B) — arrival order.
    assert_eq!(order, vec![0, 20], "A executes before B: arrival order");
    assert!(state.present_pending_exec.is_empty());
}

#[test]
fn distinct_effective_targets_never_supersede() {
    const WINDOW: u32 = 0x0001_0204;
    const A_ID: u64 = 110;
    const C_ID: u64 = 111;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();

    let mut a = SupersessionFixture::new(A_ID, WINDOW)
        .eff(Some(500))
        .geometry(0, 0, 100, 100)
        .entry();
    a.pending.masked_options = crate::present_scheduler::PRESENT_ALL_ASYNC_OPTIONS;
    state.present_pending_exec.insert(A_ID, a);

    let mut c = SupersessionFixture::new(C_ID, WINDOW)
        .eff(Some(600)) // distinct target
        .geometry(0, 0, 100, 100)
        .pending();
    c.masked_options = crate::present_scheduler::PRESENT_ALL_ASYNC_OPTIONS;
    supersede_covered_pending_presents(&mut state, &mut backend, &c);

    assert!(
        state.present_pending_exec.contains_key(&A_ID),
        "a distinct effective target must never supersede"
    );
    assert!(state.present_pending_complete.is_empty());
}

/// Scrap × window-destroy race: a present is scrapped first (fence
/// triggered, entry pin released, Skip parked), then the
/// window-destroy purge runs for the same window. The purge must not
/// find the (already-removed) entry again — no double fence trigger,
/// no double pin release. The purge's `signal_present_wake` on the
/// now-parked Skip row (a completion, not a pending-exec entry) is a
/// separate, harmless no-op backend-side; not exercised here.
#[test]
fn scrap_then_window_destroy_purge_releases_exactly_once() {
    const WINDOW: u32 = 0x0001_0205;
    const VICTIM_ID: u64 = 120;
    const SUCCESSOR_ID: u64 = 121;
    const TARGET: u64 = 500;
    const IDLE_FENCE: u32 = 0x0030_1205;
    const PIN_ID: u64 = 5;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    state.resources.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );

    let mut victim = SupersessionFixture::new(VICTIM_ID, WINDOW)
        .eff(Some(TARGET))
        .geometry(0, 0, 50, 50)
        .idle_fence(IDLE_FENCE)
        .entry();
    victim.pin = Some(PIN_ID);
    state.present_pending_exec.insert(VICTIM_ID, victim);

    let successor = SupersessionFixture::new(SUCCESSOR_ID, WINDOW)
        .eff(Some(TARGET))
        .geometry(0, 0, 100, 100)
        .pending();
    supersede_covered_pending_presents(&mut state, &mut backend, &successor);

    assert_eq!(backend.triggered_dri3_fences, vec![IDLE_FENCE]);
    assert_eq!(backend.released_present_sources, vec![PIN_ID]);
    assert!(state.present_pending_exec.is_empty());

    // Now destroy the window: the by-XID purge scans
    // `present_pending_exec` for this window and finds nothing (the
    // entry is already gone), so it must not touch the backend again.
    destroy_window_subtree(&mut state, &mut backend, None, ResourceId(WINDOW));

    assert_eq!(
        backend.triggered_dri3_fences,
        vec![IDLE_FENCE],
        "the window-destroy purge must not re-trigger an already-scrapped fence"
    );
    assert_eq!(
        backend.released_present_sources,
        vec![PIN_ID],
        "the window-destroy purge must not re-release an already-released pin"
    );
    // Review fix (5c): the destroy purge's parked-completions sweep
    // (process_request.rs, `destroy_window_subtree`'s
    // `present_pending_complete` loop) DOES call `signal_present_wake`
    // for every row matching this window — including the Skip row the
    // scrap above just parked, since scrap does not remove it from
    // `present_pending_complete`. This is a no-op on a real backend
    // (the scrapped present_id was never retained as a `PinnedWake` —
    // scrap's own by-XID release is what actually signalled the
    // client), but `RecordingBackend` records the call regardless, so
    // pin the exact expected content: exactly the victim's id, and no
    // other wake.
    assert_eq!(
        backend.signalled_present_wakes,
        vec![VICTIM_ID],
        "the purge's parked-completion sweep signals the scrapped Skip row's \
             present_id (harmless no-op on a real backend) and nothing else"
    );
}
