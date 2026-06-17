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

## Bug A — comp=ON: windows vanish on screen 2 (CURRENT FOCUS)
Default MATE runs marco WITH compositing → windows are Composite-redirected; marco composites the desktop into an off-screen pixmap and `PresentPixmap`s it to the COW (overlay window `GetOverlayWindow → 0x103`); the scene samples the COW.

**Already ruled out** (from `YSERVER_V2_SCENE_WALK_ALL=1 RUST_LOG=info,yserver::kms::v2::scene=debug` trace): the COW is geometrically correct after resize — `scene_walk xid=0x103: WILL_EMIT geom=(0,0 5120x1440) output=(-2560,0 5120x1440) storage_extent=5120x1440`, emits on output 1, and marco PRESENTs **5120×1440** pixmaps to it (not 2560). So COW size/emit/offset are all right.

**Prime suspect:** the **presented pixel content / PresentPixmap update-region**. Hypothesis: marco presents with a sub-rect *valid/update region*, and yserver mis-applies it for x>2560, so the window's pixels never land in COW[2560..5120]; OR a damage issue where output 1 doesn't repaint the window's region. Investigate yserver's PRESENT-to-COW handler (how a presented pixmap + its update region is copied into the COW storage `DrawableId`) and how output 1's compose clips to damage.

**Next diagnostic (in progress):** capture an **xtrace of marco with compositing ON on Xorg** doing the drag-to-screen-2 (the first capture `mate-xorg.xtrace` was comp=OFF — no Present requests — so invalid). Diff marco's `PresentPixmap` update-regions + Damage stream vs what marco sends yserver (yserver internal log already records `PRESENT-INSTR` / `ConfigureWindow`). Same requests on both → yserver PRESENT-to-COW/damage bug; different → we advertised something wrong. Note: xtrace may show Present/Composite as raw extension opcodes, not names.

## Bug B — hotplug "apply first" (smaller follow-up)
Hotplugging the 2nd monitor doesn't grow RANDR `screen_width` (carried forward from 1-screen boot until a client `SetScreenSize`), so marco confines *windows* to 2560 until you Apply (cursor crosses because it uses live `platform.fb_w`). = plan Task 5.2 (off-until-configured) + a `screen_size_client_set` flag so hotplug grows the bbox when no client set the size. Plan Tasks 5.1/5.2 are NOT done.

## Environment gotchas
- **Subagent spawn is broken** with `CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1` (tmux teammate backend errors `sort: unrecognized option '--agent-id'`, leaves a dead pane). Either unset that flag (→ in-process subagents work) or execute directly.
- The `yserver-mate-hw` recipe sets its own `RUST_LOG`; scene-walk env vars (`YSERVER_V2_SCENE_WALK_ALL=1`) must be on the **yserver** process line, not the mate-session line.
- HW loop: build (recipes rebuild), run, read `yserver-hw-mate.log`. comp on/off via marco compositing toggle.
