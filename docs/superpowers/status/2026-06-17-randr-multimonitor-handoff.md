# RANDR multi-monitor — session handoff (2026-06-17)

Branch: `feat/drm-hotplug` (HEAD `ebf6c665`, all pushed). Plan: `docs/superpowers/plans/2026-06-17-randr-output-management.md` (3× codex-reviewed). Continue on **silence** (RX580, 2×2560×1440) with logs local.

## What's DONE and HW-verified (on silence)
Client-driven RANDR fully implemented (plan Tasks 1.1–4.2) plus HW-driven fixes:
- Full advertised mode list + correct refresh (dot_clock = htotal·vtotal·vrefresh; was ~53 Hz).
- `RRSetScreenSize` (grows logical screen + root/COW) and `RRSetCrtcConfig` (real modeset) — both working.
- **EBUSY fix** (`87ac06c1`): resize defers to the flip-gated tick instead of drain+rebuild+present.
- **Cursor crossing** (`9c12e8a2`): `LibinputThreadState` fb extent was init-once; now re-pushed on resize/hotplug.
- **Idempotent SetCrtcConfig** (`ebf6c665`): MATE re-asserts the same config on every RRScreenChangeNotify; we were doing a full modeset + unconditional notify each time → feedback loop → single-screen flicker. Now no-op when unchanged, notify only on real change (Xorg RRTellChanged parity). **This was a serious regression; verify single-screen stays stable.**
- **comp=OFF multi-monitor WORKS**: with marco compositing disabled, windows show + drag on screen 2 (after apply). Proves the RANDR + multi-output scene stack is correct.

## Bug A — comp=ON: windows vanish on screen 2 — RESOLVED 2026-06-17 (HW-verified on silence)
Default MATE runs marco WITH compositing → windows are Composite-redirected; marco composites the desktop into an off-screen pixmap and `PresentPixmap`s it to the COW (overlay window `GetOverlayWindow → 0x103`); the scene samples the COW.

**Root cause = stale default Bounding shape on the COW.** marco resets the COW's Bounding shape to *None* (`XFixesSetWindowShapeRegion kind=Bounding region=0`) while the screen is single-head. yserver's `mirror_shape_to_host_state` (process_request.rs) then mirrored the *materialized* `default_shape_rect` — i.e. the COW's geometry at that instant, `(0,0 2560×1440)` — into the backend's `shape_bounding`. On RANDR apply the COW grows to 5120×1440, but nothing re-mirrors it, so `shape_bounding[cow]` stays the stale 2560 rect. The scene's shape-clip path (scene.rs:2442) faithfully clips the now-5120-wide COW to `[0..2560]` → `src_size=0.5`, `dst_origin=-2560` → screen 2 samples the wrong (left) half placed off-screen. Output 0 survives only because `[0..2560]` is its content. Xorg is immune: a `None` bounding region is never materialized into a rect (tracks live size).

**Fix:** `mirror_shape_to_host_state` now mirrors **empty rects** for an unset Bounding shape (backend drops the entry → scene uses the live full-window emit path). Clip/Input mirroring unchanged. Regression test `unset_bounding_shape_mirrors_empty_not_default_geometry` (+ `RecordingBackend::SetShapeRectangles` capture).

**Method that cracked it (reusable):** per-output `COMPOSE-DUMP` log of each `CompositeDraw`'s `dst_origin/dst_size/src_origin/src_size` revealed output-1's COW draw at `dst_origin=[-2560,0] dst_size=[2560,1440] src_size=[0.5,1.0]` — the exact signature of a 2560-wide bounding rect on a 5120-wide window. Ground truth came from drawable/scanout PPM dumps (**Ctrl+Alt+F12** drawable dump, **Ctrl+Alt+Enter** per-output scanout): COW storage right half = correct (cat+dialog), `out0` bit-identical to COW-left (RMSE 0), `out1` uniform gray (clear showing through). All earlier hypotheses (copy_area dst-extent clamp, PresentPixmap update-region) were REFUTED by the PRESENT-COW-COPY + PRESENT-INSTR diagnostics (marco sends x>2560 rects, copied unclamped into 5120 COW storage). All diagnostics have been removed.

## Bug B — hotplug "apply first" — Phase 5 DONE 2026-06-17 (HW-verified on silence)
Original symptom: hotplugging the 2nd monitor auto-enabled it (requery's add-loop allocated a pool + modeset + pushed to `platform.outputs`), so Display CC reported it **on** while it was dark, and `screen_width` didn't reflect a real client-driven layout.

- **Task 5.1 (preserve client-configured layout):** `recompact_horizontal_layout` now takes the set of `client_configured` connector names and skips them (leaves their `(x,y)`), packing only auto outputs past pinned extents. `requery_outputs_and_modeset` threads the set; both callers (`run_resume`, `run_display_rescan`) build it from `RandrIdAllocator::client_configured_names()`. Client layout survives VT-resume + rescan instead of flattening to boot extend-right. Core test `from_outputs_preserves_nonzero_y_position`.
- **Task 5.2 (hotplug registers off):** runtime rescan no longer auto-enables a new connector — no pool/modeset, not in `platform.outputs`; surfaced in `RescanResult.added_outputs`. `fire_randr_changes` reconciles the registry (dropped→disconnected, added→`connected=true, config=Off`, modes recorded). `randr_outputs_and_modes` not-live branch reports `connected=<registry flag>` so a hotplugged-off output is RR_Connected/mode=0 ("connected, dark"), a physically-absent one RR_Disconnected. Boot still auto-enables via `open_with_commit`. Backend test `not_live_output_reports_connected_off_vs_disconnected`.

HW-verified: hotplug → screen 2 stays dark + CC shows it connected/off; enable in CC → lights up + windows work (Bug A fix); VT-switch preserves the configured layout.

## Environment gotchas
- **Subagent spawn is broken** with `CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1` (tmux teammate backend errors `sort: unrecognized option '--agent-id'`, leaves a dead pane). Either unset that flag (→ in-process subagents work) or execute directly.
- The `yserver-mate-hw` recipe sets its own `RUST_LOG`; scene-walk env vars (`YSERVER_V2_SCENE_WALK_ALL=1`) must be on the **yserver** process line, not the mate-session line.
- HW loop: build (recipes rebuild), run, read `yserver-hw-mate.log`. comp on/off via marco compositing toggle.
