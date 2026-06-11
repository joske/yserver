# Known Issues

Cross-cutting bugs, limitations, and dev-loop friction that surface
during validation, debugging, or interactive use. Phase-bound feature
work lives in [`status.md`](status.md) under each phase's follow-ups
section; this file is for issues that don't fit a phase or aren't
worth a phase of their own.

Add items as you find them. Tick them off when fixed. Prefer concise
entries with enough context for a future debugging session to start
from.

## Input, grabs, event routing

- [~] **GTK wheel scroll needs another app to open first (residual
      after XI2-valuator-scroll fix).** Largely fixed 2026-05-15 by
      adding XIScrollClass + scroll valuators 2/3 to the master
      pointer's XIQueryDevice reply, emitting XI_Motion events with
      cumulative scroll-axis values on each wheel click, and
      reporting the current counter as the axis's `value` field so
      mid-session clients pick up the right baseline. Caja now
      scrolls from the first click in its default view.
      Residual: the FIRST app a yserver session opens
      occasionally doesn't scroll on its initial scrolls. The
      trigger to unstick is **any scroll event reaching any GTK
      target** — scrolling on the MATE desktop (caja-managed)
      counts even though it produces no visible scroll. After
      that, scrolling works everywhere for the rest of the
      session. Non-deterministic: across three test runs, two
      needed the warm-up and one worked first try.
      Wire is delta-correct now (Relative mode, ±1 per click,
      NoEmulation flag, valuator 2/3 declared). The race appears
      to be between yserver emitting XI_Motion-with-scroll-axis
      and GDK's XI2 device-init handshake completing — if GDK
      hasn't finished caching the device's scroll classes when
      the first event arrives, the event is dropped. Real Xorg
      may avoid this by sending some initial axis state with
      XI_HierarchyChanged or by serializing XIQueryDevice replies
      against subsequent events.
      User workaround: scroll once on the desktop or in any
      other GTK app to prime. Filed for follow-up; not worth
      blocking on.
- [~] **Cinnamon click-to-focus replay path was incomplete on the
      XI2 side.** Surfaced 2026-05-26 while validating
      `cinnamon-settings`: the settings window activated on click,
      but the clicked row/button never received the press. Trace
      logs showed the focus-grab owner immediately sending
      `XIAllowEvents(deviceid=2, mode=ReplayDevice)` after each
      activation click. The first fix wired ReplayDevice through the
      core replay helper, but the deeper bug was that
      `XIPassiveGrabDevice` had been recorded with `pointer_mode=1`
      unconditionally, so muffin's sync passive grabs never froze the
      ButtonPress in the first place. yserver therefore leaked the
      XI2 press to the unfocused target before focus activation and
      had nothing useful to replay afterward. Fixed in the current
      tree by preserving XI2 passive-grab modes, withholding the
      initial XI2 ButtonPress from non-grab-owner clients when the
      grab is synchronous, and replaying the XI2 press to the natural
      target on `XIAllowEvents(mode=2)`. Added a regression test that
      models the full sequence. The earlier attached-slave XI2
      topology cleanup remains useful hardening for Cinnamon/GTK, but
      it was not the direct cause of the dead clicks. Follow-up trace
      data exposed a second bug in the focus path: once muffin
      focused the toplevel, `cinnamon-settings` moved focus into an
      inferior child window and yserver reported that as a generic
      top-level focus loss (`FocusOut detail=NotifyAncestor`) instead
      of the correct ancestor-chain transition (`NotifyInferior` on
      the parent, `NotifyAncestor` on the child). Cinnamon then
      cleared `_NET_ACTIVE_WINDOW`, so the focused window looked like
      it was deactivating under the pointer on every click. Current
      tree fixes both core and XI2 focus fanout to use real
      ancestor-chain details and keeps the child as the active
      keyboard focus; regression coverage was added for the
      top-level -> child move. A follow-up working Xorg `x11trace`
      showed the remaining click stream now matches closely on the
      wire, so routing/replay/focus are no longer the obvious
      blockers. The clearest remaining mismatch was protocol
      negotiation: yserver still advertised XI 2.2 while Xorg
      negotiated XI 2.4 for the same clients. Current tree now
      negotiates `XIQueryVersion` like Xorg for a 2.4-capable
      server, with regression coverage for 2.4/2.3 replies. Another
      Xorg diff then exposed a second metadata gap: GTK selects
      `XI_DeviceChangedMask` on the root window and Xorg immediately
      sends a master-pointer `XI_DeviceChanged` event with labeled
      button/valuator classes; yserver had been sending none, and its
      `XIQueryDevice` pointer classes were still unlabeled. Current
      tree now bootstraps that `XI_DeviceChanged` event and labels the
      virtual pointer classes with Xorg-style atoms. Build/test
      validation is green.

      **Cinnamon runtime re-test 2026-05-26 PM — input path now
      verified good, symptom moved.** Captured one-click trace of
      `cinnamon-settings applets` clicking on the Themes category.
      The XI2 press reaches `cinnamon-settings` cleanly (client 052
      in the xtrace, window 0x3500006, single press + single release
      at the correct coords). GTK's click handler runs through to
      completion on the app side: `GrabServer` / `XIQueryPointer`×3 /
      `UngrabServer` / `GetInputFocus` / `XIChangeCursor`, then
      `ChangeProperty WM_NAME = "System Settings > Themes"`, then 7×
      `MIT-SHM::Attach` for the new panel's buffers. muffin observes
      the property change and repaints the decoration — the user
      sees the **title bar update to "System Settings > Themes"**.
      But `cinnamon-settings` issues no further draw calls after the
      SHM attaches — no `PutImage`, no `RENDER::Composite`, no
      `CopyArea` to window 0x3500006. The user sees the **categories
      grid still rendered**; only the title changed. Symptom is
      identical with a 2 s post-click wait and with a longer wait,
      so it is not just panel-load latency.

      Conclusion: **this is not a yserver input / focus / protocol
      bug anymore**. cinnamon-settings's Python click handler is
      hanging between "title updated + SHM attached" and "draw new
      panel". Suspects (none confirmed): a blocked DBus call from
      the panel-load code (color-manager, accountsservice, polkit),
      a slow `Gtk.IconTheme` / theme enumeration, a GSettings read
      that hangs because the underlying daemon misbehaves under
      yserver. Open follow-up: capture a Python stack via
      `py-spy dump --pid <cinnamon-settings-pid>` (or
      `strace -p <pid> -e trace=recvmsg,poll` for the syscall view)
      while the broken state is on screen. That will name the
      blocked frame and we can decide whether it's a yserver-visible
      bug after all (e.g. a missing event the daemon waits on) or
      strictly a Cinnamon-side issue worth handing back to Cinnamon
      upstream. Until then, downgrade priority — MATE is the
      validated desktop, Cinnamon's click activation works on the
      wire and the next step is process-side diagnosis.
- [ ] **`UnmapNotify.from_configure = true` never wired.** Encoder
      accepts the byte for wire correctness; every call site currently
      passes `false`. The `true` path fires when a parent's
      `ConfigureWindow` shrinks a child out of view. Wire once we
      track parent-resize-driven implicit unmaps. (Phase 1 follow-up.)
- [ ] **`RRSelectInput` mask storage.** `RRSelectInput` is accepted
      but the mask is not stored; `RRScreenChangeNotify` is never
      delivered to RANDR clients. (Phase 2 follow-up.)
- [ ] **`GrabButton` Sync mode replay.** Current implementation stores
      the frozen event but `AllowEvents(ReplayPointer)` requires access
      to the `xid_map` which (pre-Phase-6.3) lived on the pump thread.
      Phase 6.3 moved `xid_map` into `HostX11Backend`; revisit whether
      replay is now wirable, or whether it still needs an inter-thread
      channel. (Phase 2 follow-up.)
- [ ] **`SendEvent` parent-tree propagation.** Current impl delivers
      to direct window subscribers; does not propagate up the window
      tree (`event_mask=0` with `PointerWindow` / `InputFocus`
      destinations). (Phase 2 follow-up.)
- [ ] **Input-shape hit testing in pointer pump.** Deferred since
      Phase 3.2. SHAPE input rectangles are stored locally but pointer
      hit tests don't honor them — clicks land on shape-rejected
      regions when they should pass through.
- [ ] **`CreateCursor` `XColor` struct layout.** Xlib `XColor` layout
      must match the system Xlib headers; verify on non-CachyOS target
      platforms. (Phase 2 follow-up.)
- [~] **Crossing-event `child` field + detail codes.** Partially
      fixed 2026-05-15. The protocol-level wire issues that this
      entry originally claimed (no `child` field, hardcoded
      `detail=0`) are addressed: `CrossingEvent` now has a `child`
      field, encoder writes it, and `update_pointer_window`'s chain
      walk computes proper detail codes
      (NotifyInferior/Ancestor/Virtual/Nonlinear/NonlinearVirtual)
      via `crossings::normal_mode_crossings`, plus
      `crossings::implicit_grab_crossings` now also populates
      `child` per spec.
      Original repro (e16 hover popup fires over xterm) is **not
      fixed** by these changes. x11trace analysis showed e16 IS
      selecting Enter/Leave + Motion on root, and is receiving
      Enter events with correct detail codes after the fix — but
      the popup still fires. Whatever signal e16 uses for the
      "click on desktop" hover gating isn't load-bearing on
      Enter/Leave-with-detail semantics in a way these changes
      affect. Needs a side-by-side wire-trace comparison against
      real Xorg+e16 to identify the actual signal e16 relies on
      (candidate: motion-event delivery semantics for embedded
      widgets, or a XEmbed-protocol interaction). Deferred.
      Followup gap noted during investigation: yserver's
      `update_pointer_window` only emits crossings on **top-level**
      transitions. Per X11 spec, when cursor enters a top-level
      with descendants, Enter events should fire on EACH window in
      the chain (deepest descendant → ancestors), with detail codes
      indicating position-in-chain. yserver instead fires only on
      the top-level; for now the propagation walk in
      `pointer_fanout::pointer_propagation_target_by_id` covers the
      common case (subscribers on ancestors get events from
      descendants via walk-up). This is non-spec-compliant but
      practically works for xfce/mate/marco. Re-do as deepest-target
      tracking if a real client surfaces a bug here.
## Drawing / rendering artifacts

- [ ] **nemo desktop rubber-band selection renders a stepped
      (staircase) leading edge during the drag (Cinnamon, bee,
      2026-05-27).** Exposed once the rubber-band anchored correctly
      (after the `XIGetClientPointer` deviceid fix, `967c09d`). While
      dragging a marquee selection on the nemo desktop, the selection
      rectangle's trailing/top/left edges are clean but the **leading
      (right/bottom) edge is a diagonal staircase of translucent
      steps** — old band edges from prior frames remain visible. **On
      button release it briefly shows the correct, clean rectangle**
      before the band clears.
      Evidence (captured drawable dump
      `yserver-v2-drawable-0-present-src-0-*.ppm`, this is the
      composited scanout source): the staircase is in the composited
      output itself.
      Diagnosis: NOT geometry and NOT the client's output. nemo
      double-buffers — it renders desktop+band into a backing pixmap
      and `CopyArea`s the band bbox to the desktop window each frame
      (trace shows large per-frame copies, e.g.
      `dst=(7,0) 627x496`), so nemo's source is a clean rectangle. The
      "correct on release" behaviour proves the backing is correct and a
      **full** repaint renders it right; only the **incremental** repaint
      during the drag is wrong. This points at v2's Stage 2e buffer-age
      clipped repaint: with 2-3 scanout buffers, each buffer must repaint
      the damage accumulated since *it* was last drawn, not just the
      latest frame's sliver. If the moving band's window-content damage
      is only applied for the current frame, the buffer retains band
      pixels from ~2 frames ago in the uncovered gap — and that
      stale edge is exactly one "step". Release issues a full-region
      redraw, so every buffer gets clean content → correct briefly.
      So far **nemo-specific**: caja (MATE, same nautilus rubber-band
      code) does not show it on yserver — possibly because caja's band
      is opaque/XOR rather than a translucent overlay, so stale pixels
      don't read as a visible step. Worth confirming caja doesn't
      staircase under close inspection.
      Next step: trace the v2 damage path for a moving sub-region —
      confirm whether window-content damage from `CopyArea` / RENDER
      fills is accumulated across buffer generations in
      `pick_repaint_region` / the `BufferAgeRing`, or only applied to the
      current frame. Fix is almost certainly in that accumulation, not
      in the input/protocol layer.

- [ ] **Follow-up: replace the present-source CPU wait with a GPU
      acquire-semaphore (perf).** `wait_present_source_ready`
      (backend.rs) CPU-`poll()`s the source dma-buf's read fence before
      the present copy — correct and bounded, but it stalls the
      single-threaded core for the producer's outstanding render
      (usually sub-frame; can add up under many concurrent GPU clients).
      The non-stalling form imports that same fence as a `VkSemaphore`
      (machinery already exists: `vk::sync::import_sync_file` +
      `external_semaphore_fd`) and waits on it on the present copy's
      **submit**. Land it with the composite-into-frame-builder work
      (Stage 5 / Phase B): that gives one well-defined submit per frame
      to carry the acquire list, with the semaphore's lifetime tied to
      the frame's fence ticket. NB Vulkan queue waits have no timeout —
      only ever wait when `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` returns a real
      fd, else no-op, to avoid a never-signalled-wait hang.

- [ ] **Stage 4c follow-up: scene-structure damage rects clip in
      the wrong frame on non-origin outputs (low impact).**
      `SceneCompositor::mark_scene_structure_damage_rects`
      (scene.rs:653 → `dispatch_clip_rects_to_outputs`) clips each
      screen-absolute input rect by `output_extent` measured from
      (0,0), ignoring the output's layout origin. The rest of the
      pipeline IS layout-aware (`build_scene` per-output
      `layout_x0/y0` translation, cursor placement,
      `add_projected_damage` projection), so multi-output itself
      works — confirmed by weeks of dual-screen daily use on
      silence (2026-06-12). Blast radius is confined to the
      helper's sole call site, `set_window_scene_participation`
      (backend.rs:9742): when a window's COMPOSITE redirect
      participation flips (compositor start/stop, redirect
      toggle), the precise damage rect for a window on a
      non-origin output is dropped or misplaced. Worst case is a
      transient stale rect there, normally masked because all
      other structure changes use the full-output
      `mark_scene_structure_dirty` and later content damage
      repaints the area. Fix: (a) add `layout_origin:
      vk::Offset2D` to `OutputSceneState` and clip in the right
      frame, or (b) translate at the call site. Low priority; no
      user-visible repro to date.

- [ ] **Stage 4c follow-up: `src_size = [1, 1]` after redirected W
      resizes past B's extent.** `build_scene`'s redirect-aware
      indirection (Stage 4c.3, commit `b9a9f3a`) emits
      `CompositeDraw.src_size = [1.0, 1.0]` (normalized UV "sample
      whole texture") for the W entry. When W is Automatic-redirected
      to backing B and W later RESIZES past B's extent, the scene
      stretches B onto W's new larger `dst_size` instead of leaving
      the now-uncovered region undefined. Conversely on W-shrink the
      scene samples a B-region larger than W now is, with the same
      undefined result. Either: (a) resize B in lockstep with W (per
      Composite spec NameWindowPixmap aliases freeze at pre-resize
      content so this is delicate), or (b) clamp `src_size` to the
      W/B extent ratio. Plan §4c didn't pin a behaviour; revisit
      before the 4c hardware smoke gate (mate + marco) or risk
      stretched window content on resize.

- [ ] **xeyes resize-DOWN artefact on v2 + mate + marco
      (2026-05-17, open).** Continuous-drag-shrink leaves xeyes
      with eye geometry sized for a *wider* window than what's
      now displayed — eye 2 visibly cut off at the right edge.
      Resize-UP works clean after the 2026-05-17 fix chain
      (MaskScratch swizzle + trap-shader AA + xid-detach +
      pixmap zero-fill). Two hypotheses, neither confirmed:
      1. xeyes' eye-geometry state stale during rapid drag —
         pixmap dims caught up but draw geometry didn't. Verify
         by stopping the drag and waiting 2-3 seconds before
         dumping the scanout; if the eyes settle to correct
         smaller shape, it's an xeyes-side race.
      2. v2 scene compositor blits the wrong storage — perhaps
         a pending-ack carrying an old DrawableId. Less likely
         given the test coverage, but worth re-checking the
         `pending_acks` capture path.
      Also visible (orthogonal): many `render_composite gap:
      host_src 0x40xxxx not resolvable` lines from marco's
      decoration compositing, depending on `name_window_pixmap`
      stubbed `Err` on v2 (Stage 4). Real fix is Stage 4 — the
      gaps aren't a v2 regression, but the noise complicates
      diagnosis here.
- [~] **MATE panel flicker on v2 (2026-05-17).** Reported with the
      resize-down session above. **Likely resolved 2026-06-01** by the
      systray ClipByChildren fix below — the speculated "rapid panel
      applet activity" cause matches the notification-area damage storm
      (~485 FillRectangles/socket per vsync), which recomposited the
      panel strip every frame. Re-confirm on a MATE session; if flicker
      persists outside the tray region it's a separate scene-damage
      issue.

- [ ] **Tray icons intermittently blank / bottom-sliver — manual-redirect
      paint-routing race (2026-06-01, open; still reproducing as of
      2026-06-12).** Distinct from the storm fix
      above: after `e7a1ba0` icons *can* render, but intermittently come
      up blank or as a thin bottom line; varies run-to-run (1–3 icons
      affected). `YSERVER_TRAY_DEBUG=1` proved it's NOT geometry/clip —
      socket 26×27, plug child 26×27@0,0 Viewable, backing 26×27, and the
      panel's Clear is correctly a no-op (`clipped=[]`). The icon is blank
      because the plug's paint doesn't land in the socket's redirect
      backing *that the panel reads*. The initial paint-routing-race
      hypothesis was **refuted** by probe (`yserver::kms::v2::paint=trace`:
      `NO_REDIRECT_FOUND=0`, `REDIRECT_FOUND=116` — plug paints DO route to
      a backing). So capture works; the bug is **downstream in the
      read/composite of the backing**. Leading candidates: (1) **source-read
      asymmetry** — the panel reads each socket as a Composite SOURCE; if
      that samples the live (empty) socket not the backing → blank. This is
      what the parked `fix/applet` `resolve_source_picture` redirect-walk
      fix addressed (it "didn't fix icons" then only because the storm was
      still wiping; revisit NOW on top of `e7a1ba0`). (2) **backing rotate**
      — sockets alloc `1×1` then grow `26×27`; if rotate doesn't recapture,
      a read around it could yield artifacts. NOTE (user, 2026-06-01): the
      failing icon is **fully blank**, NOT a clipped/sliver icon — a
      correctly-drawn applet has no bottom line. The "1px bottom line" seen
      in failures is a SEPARATE minor artifact (stray composited row /
      edge), not a partial icon. Blank-not-partial favors candidate (1)
      (source-read returns empty live socket) over (2). scene_walk
      `skip_reason="no_intersect_with_output"` decisions also worth
      auditing. NEXT: rebase parked `fix/applet`
      `resolve_source_picture` onto `clip-by-children` and re-test;
      instrument source-read + backing-rotate if not. Fragile/black-screen-
      prone subsystem — HW smoke before commit. Banked traces:
      `mate-working.xtrace` (all 3), `mate-sliver.xtrace` (failing). Diag
      (`YSERVER_TRAY_DEBUG`) in working tree, marked REMOVE.

- [ ] **Per-client GC mirroring** (Phase 3.7 task #26). The shared
      host GC creates subtle bugs when GC state leaks between clients.
      Phase 3.7's fill-style fix needed careful "reset to Solid after
      each draw" to avoid tile state bleed; Phase 6.3 added similar
      careful state management for `function`/`plane_mask`/etc. via
      `apply_draw_state`. Real per-client GC mirroring would
      eliminate the reset-after-draw discipline. ~Medium scope.
- [ ] **Sub-window Expose for off-screen / behind-sibling drags.**
      Backing-store mitigation from Phase 3.6 covers the common case;
      synthetic Expose for the corner cases (window dragged fully
      off-screen / fully behind another) is the proper fix and is
      deferred. Re-open if a real validation scenario demonstrates a
      backing-store gap.
- [ ] **`render_set_picture_filter` honoured only as Nearest.**
      v1: `render_set_picture_filter`
      (`crates/yserver/src/kms/backend.rs:12392`) accepts the
      request but ignores the filter — Vk Composite shader uses
      a fixed sampler. v2: filter is stored on
      `PictureRecord::Drawable` (3b) but the engine still
      samples Nearest regardless of the requested filter
      (`Bilinear` / `Convolution` deferred per spec § "Out of
      scope" — Stage 5 perf work). Clients that depend on
      `Bilinear` for picture-source scaling see Nearest output.
      Stage 3 status quo per the Stage 3 plan §"Non-goals" #8.

- [ ] **KMS: `MapSubwindows` doesn't re-Expose deep descendants
      promoted by the map_window viewable cascade.** After commit
      `304858f` (`fix(resources): propagate Viewable down through
      Unviewable descendants on map`), mapping a window correctly
      flips deep descendants from `Unviewable` to `Viewable`.
      `handle_map_window` already calls
      `emit_expose_subtree_to_state` which walks descendants, so
      that path is fine. But `handle_map_subwindows` only emits
      MapNotify+Expose for the children it directly maps, not for
      their grandchildren that may have just been promoted by the
      cascade. Edge case (most clients only have one level of
      MapSubwindows children) but spec-incorrect. Fix: in
      `handle_map_subwindows`, after the per-child loop call
      `emit_expose_subtree_to_state` on each child whose viewability
      transitioned. ~10 LoC.
- [ ] **Damage accumulation on RENDER drawing ops.** Phase 3.5's
      first-cut `accumulate_damage` covers core drawing only.
      RENDER-driven damage (composite, fill rectangles, glyphs) is
      not accumulated. Matters once a real client (compositor /
      screen recorder) drives the path.
- [ ] **PictFormat tracking + ARGB picture intent (Stage 4e
      candidate).** Surfaced as residual from Stage 4 close
      (2026-05-21). v2 forces opaque on depth-24 source pictures
      (`d20f279`) and routes xRGB/RGB24 through the resolver as
      forced-opaque, but doesn't track per-picture PictFormat
      semantics (xRGB-as-render-intent vs ARGB) end-to-end. Full
      compositor-WM correctness (consistent ARGB-aware shadow /
      transparency / multi-layer Render::Composite) needs
      PictFormat tracking on `PictureRecord::Drawable` + the
      backend sampler, plus picture-source alpha interpretation
      per format. Until then the cow-authoritative scene gating
      keeps the compositor's path working but full Render
      Composite correctness in complex DE chrome (e.g. tooltip
      drop-shadows blending against arbitrary backgrounds) is
      not guaranteed. Marked candidate "Stage 4e" in earlier
      planning; not on the Stage 5 perf path.

      **Resolved sub-investigation (2026-05-22 → 2026-06-04):**
      the gtk3-demo / MATE-tooltip CSD shadow divergence vs Xorg
      (denser dark band, opaque black bar under the tooltip) that
      was tracked here turned out to be a Trapezoids source-origin
      bug, not a PictFormat issue: `render_trapezoids` /
      `render_triangles_op` ignored `xSrc`/`ySrc` and the two-stage
      GPU trap path reused a stale, bbox-only-cleared
      `mask_scratch`, collapsing GTK's blur-ramp shadow mask
      toward a solid 50%-black slab. Fixed (HW-verified) by
      threading the client src origin through the trap path
      Xorg-style and clearing the full scratch for `op=Src` /
      full-dst trap ops — merged to master (`b0a5084`).
      One residual observation from that investigation remains
      unexplained: the wallpaper green channel differed by 4 LSB
      vs Xorg despite an identical wallpaper file (suspects:
      `PutImage` depth-24/32 byte handling, visual / PictFormat
      advertisement steering Cairo into a different path —
      yserver advertises ~5 visuals vs Xorg's ~272).
      Re-investigate if a visible colour divergence resurfaces.
- [ ] **MATE Control Center yellow group-header labels missing
      (mate-no-comp).** Group headers in the Control Center
      sidebar ("Filter", "Groups", "Common Tasks") render
      invisible. Reproducible without a compositor, so not a
      COW/redirect bug. Likely a colored-source + glyph-mask
      Render::Composite path issue — yserver's Render glyph
      compositing with a non-default source colour is the most
      plausible suspect. Surfaced from the Stage 4d hardware
      smoke (now archived).

## wmaker on KMS

- [ ] **wmaker title-bar close/minimize button glyphs missing.**
      Same general area as the appicon (CWBackPixmap-driven small
      icons) but the depth-1 PutImage fix didn't cover it. The
      buttons render as plain coloured 25×25 squares without their
      X / − glyphs. Probably a different drawing path — wmaker may
      be using PolySegment / PolyLine to stroke the glyphs after a
      ClearArea, and either the glyph strokes are clipped or our
      drawing primitives don't honour something they need. Cosmetic;
      drag/move/close-via-menu still work.
      wmaker comes up on KMS and draws its dock/clip; xterm under
      wmaker connects, sends MapRequest, gets reparented into a
      wmaker frame, gets configured to fit, and renders text into
      its (now-hidden) backing pixmap — but the frame itself is
      never mapped, so xterm is invisible.

      Trace from client 0 (wmaker) shows the typical sequence:
      `GrabServer` → frame `CreateWindow` → frame-children
      `CreateWindow` + `MapWindow` → long restack chain across all
      existing top-levels → frame `ConfigureWindow` (geometry + border
      width) → `ReparentWindow` xterm into frame → `ConfigureWindow`
      xterm to fit. After that point wmaker continues drawing
      decorations and creating an appicon, but no `MapWindow` is
      ever sent on the frame xid (0x10051b in the captured run) and
      no `MapWindow` on xterm's own xid follows the reparent.

      wmaker also logs `internal X error: 0 Request code: 0 DUMMY
      Request minor code: 2 Resource ID: 0xff000000 Error serial:
      10172` early — the resource id `0xff000000` doesn't match any
      xid yserver allocates and `code: 0` isn't a real X11 error
      code, so wmaker is either mis-parsing one of our replies or
      reacting to something our error encoder produced with a wrong
      `code`/`major_opcode`/`bad_value` triple. That error happens
      well before xterm starts, so it may or may not be related to
      the missing `MapWindow`.

      fvwm3 (`yserver-fvwm3-xterm` recipe) is fully working;
      wmaker's flow has different expectations that we don't meet.

      **Diff against ynest (where wmaker+xterm works):** in ynest,
      wmaker's MapRequest handling continues past frame creation to:
      ```
      CreatePixmap 64x64 (appicon)
      MIT-SHM::PutImage (upload appicon image)
      MIT-SHM::Detach
      ChangeSaveSet (on the xterm window)
      MapWindow frame
      MapWindow xterm
      ```
      In yserver, wmaker stops before any of these — the `ChangeSaveSet`
      / `MapWindow frame` / `MapWindow xterm` lines are entirely absent
      from the trace, and the appicon MIT-SHM upload for the new
      window never happens (MIT-SHM count: ynest 430, yserver 409).
      Combined with the early `internal X error: 0 ... Error serial:
      10172` wmaker logs to its catchXError handler, the most likely
      story is that we're sending wmaker a malformed error reply
      somewhere during frame setup, and wmaker's XSetErrorHandler
      branches into a "skip mapping" recovery path. Bisect candidates:
      the long restack chain (~150 ConfigureWindow with stack_mode +
      sibling), one of the SHAPE::SelectInput / SHAPE::QueryExtents
      probes, or a CWCursor / CWBorderPixmap attribute we silently
      drop in change_subwindow_attributes.

## KMS backend (Phase 6.4 / 6.5)

Surfaced while bringing up xeyes / xterm / xclock and fvwm3 against
`KmsBackend`. The backend is the bare-metal counterpart to
`HostX11Backend` — primitives go straight to a pixman scanout buffer
instead of a host X server, so gaps in our rasterisation surface here
that the host hides for us.

- [ ] **Crash before `console::Drop` leaves the host TTY unusable.**
      yserver's startup grabs the VT with `KDSKBMODE=K_OFF +
      KD_GRAPHICS` (kernel keystroke→TTY translation suppressed) and
      relies on the matching restore in `console::Drop` for cleanup.
      Any abnormal exit that bypasses `Drop` — SEGV/SIGABRT, an
      unhandled signal whose default action is "terminate"
      (e.g. SIGUSR1 on a binary without our handler), DPMS-induced
      power-save with the box still alive, etc. — leaves the kernel
      in K_OFF on that VT. From then on Ctrl+Alt+Fn does nothing,
      the VT can't be switched away from, and the screen looks
      frozen even though the box is responsive over SSH. Recovery
      from a remote shell: `sudo systemctl restart display-manager`
      (gdm/sddm/lightdm) — clears the state and gets you back to
      Wayland/Xorg login. Hardening: install a SIGSEGV/SIGABRT
      signal handler that explicitly calls the console restore
      before re-raising, and propagate the K_OFF restore through
      panic hooks too.
- [ ] **Clean MATE logout leaves the console wedged.** Surfaced
      2026-05-26 on M2 Asahi (Apple Silicon, AGX-V) and reproduced
      on silence (i9 13900k + RX580 / GCN4 / RADV). Workflow: launch
      MATE via `just yserver-mate-hw…`, log out via the MATE session
      menu (clean session-end, not a zap). Expected: yserver
      `Drop` runs, console returns. Observed: screen stays on the
      last yserver frame, **`chvt` hangs** when issued from SSH,
      `sudo kbd_mode -a` does not restore input, only a reboot
      recovers. `dmesg` is clean (no DRM/AGX fence timeouts, no
      `*ERROR*`), so the GPU pipeline is not wedged — the failure
      is in userspace VT state. Both affected machines have
      vendor-modern DRM drivers (Apple AGX kernel + Mesa AGX-V on
      one, amdgpu/RADV/GCN4 on the other), suggesting it's not a
      single-vendor quirk; more likely `console::Drop` /
      `disable_output` is not getting all the way through when the
      caller exits via the session-end path. Hypothesis to verify:
      mate-session's terminating yserver doesn't go through the
      signalfd `Message::Shutdown` drain (so `disable_output` /
      `ConsoleGuard::Drop` partial-fires), OR fbcon was unbound at
      startup and isn't being rebound on this exit path. Repro is
      stable on both machines so a single diagnostic run with
      strace + `/sys/class/vtconsole/vtcon*/bind` capture
      pre/post-exit should pin it down. Recovery for now:
      SysRq SAK (`Alt+SysRq+K`) kills the wedged session on the
      current VT and rebuilds the console — works on silence
      (standard PC keyboard with a SysRq key). On the MacBook (no
      dedicated SysRq, the Fn-key remapping fights the combo), SAK
      is impractical and the only recovery is `sudo systemctl
      reboot` from SSH. Distinct from the SIGSEGV / SIGABRT
      case above — this is a *clean* session-end, not a crash.
- [ ] **Atomic `disable_output` returns EINVAL on AMD Polaris
      (residual bug after the P0 fix).**
      `crates/yserver/src/drm/modeset.rs:387` builds a single atomic
      commit that clears plane FB_ID/CRTC_ID, sets CRTC ACTIVE/MODE_ID
      to 0, and clears connector CRTC_ID. The kernel rejects this
      with `-EINVAL` on both DP-1 and HDMI-A-1. The teardown disarm
      path makes this harmless (failed outputs leak their FBs instead
      of being destroyed, so dms recovers), but the warning still
      fires on every shutdown and the leaked handles persist until
      DRM-fd close at process exit. Two likely causes per codex's
      original diagnosis:
      1. `MODE_ID = 0` on a blob property may need the property
         unset specifically rather than set to integer 0.
      2. Some drivers want the plane cleared in a separate atomic
         commit BEFORE the CRTC is deactivated.

      Fix recipe: split the single atomic commit into per-stage
      commits — (a) clear plane FB_ID + CRTC_ID, (b) deactivate
      CRTC + clear MODE_ID blob, (c) clear connector CRTC_ID. Each
      with `ALLOW_MODESET`. Cross-reference Mesa libdrm fallback or
      Xorg's `drmModeAtomicCommit` deactivation sequence. Medium
      priority — no user-visible regression today; cleaner shutdown
      logs + zero-leak success path are the goals.
- [ ] **xeyes-on-e16 window drag is sluggish on bare HW.** Observed
      on a real DP-attached AMD card via `yserver-e16-...-hw` (KMS).
      During a drag the dragged frame visibly lags the cursor.
      `yserver-hw.log` over the drag window shows the compositor
      saturating vblank (pageflip-complete + composite resubmit
      pairs back-to-back at ~60 Hz) but only ~20 `ConfigureWindow`/s
      reaching the WM despite continuous pointer motion — input
      delivery, e16 throttling, or composite wall-time per frame
      could each be the bottleneck and the log alone doesn't
      distinguish them. To diagnose: add timestamped tracing on
      (libinput motion → MotionNotify dispatched) and (composite
      start → flip submitted) for one drag, then correlate. The
      compositor never falls behind vblank in the log, so the most
      suspicious link is input-event delivery rate.

- [ ] **mate-control-center under adapta-nokto: catastrophic mouse
      lag.** Observed 2026-05-13 on `bee`. The moment mate-cc is
      open under the adapta-nokto GTK theme, the whole desktop becomes
      unusable — Alt-F2 takes minutes to surface, cursor lags by
      minutes behind the actual input. `amdgpu_top` shows continuous
      GPU work, so this is real submission saturation, not yserver
      spinning on CPU. Closing mate-cc (or switching to the default
      Menta theme) restores responsiveness — under default theme
      mate-cc is mostly OK, only briefly laggy. Reproduces on `master`
      (predates Phase 3F-1), so this is not a 3F-1 regression. The
      hardware angle is unconfirmed — adapta-nokto was not tested on
      `silence`, so we don't yet know whether the cliff is
      hardware-class-dependent or just absolute theme-workload-driven.

      **Post-3F-2 update (2026-05-13)**: 3F-2 retired
      `try_vk_render_traps_or_tris`'s per-call `vkQueueWaitIdle`
      (the traps half of the original two-part hypothesis), but the
      lag is **unchanged** — adapta-nokto + mate-cc still brings the
      machine down to the point where `amdgpu_top` (a separate process,
      unrelated to yserver) stops redrawing. That confirms the
      bottleneck is GPU-submission saturation, not input-loop
      starvation, which in turn rules in the remaining hypothesis:
      `GlyphAtlas::intern`'s per-glyph `queue_wait_idle` is the
      dominant remaining cost (each new glyph drains the queue
      completely before the next can be uploaded). Phase 5 scope.

      **Post-Phase-5 / pool / GPU-trap update (2026-05-15)**: Phase 5
      sync rework, the pixmap pool, and GPU trap rasterization all
      landed; perceived lag on fuji (Intel) improved dramatically.
      `bee` (AMD RDNA2) is "no GPU faults but laggy" per status.md.
      `silence` (powerful machine with discrete GPU) **still
      reproduces the catastrophic mate-cc + adapta-nokto cliff**,
      which rules OUT "the slow machine just can't keep up" — the
      absolute submission count or per-frame work exceeds the
      budget even on capable hardware. Remaining suspects:
      `GlyphAtlas::intern` per-glyph wait still present (filed
      separately); RENDER Composite path's per-op submission rate
      under heavy GTK-glyph workloads; or a higher-level frame
      management gap that lets one workload starve subsequent ones.
      Cross-vendor reproduction (Intel-fuji OK, AMD-bee residual,
      AMD-silence catastrophic) suggests hardware/driver factors
      compound on top of an absolute-rate problem.
- [ ] **Vulkan validation error: `shaderDemoteToHelperInvocation`
      SPIR-V capability declared but device feature not enabled.**
      Surfaced 2026-05-31 during the MIT-SCREEN-SAVER smoke run on
      bee (AMD RDNA2): `vkCreateShaderModule()` validation reports
      `SPIR-V Capability DemoteToHelperInvocation was declared, but
      one of [...] VkPhysicalDeviceVulkan13Features::
      shaderDemoteToHelperInvocation [...] is required`. yserver
      passes Vulkan-1.3 features but does not query and enable
      `shader_demote_to_helper_invocation`, while at least one
      shader (saver-overlay-related path triggers it; possibly any
      with `discard;`) declares the SPIR-V capability. Validation-
      only on bee's driver (RADV happily creates the module),
      but a stricter ICD would reject the module. Fix in
      `crates/yserver/src/kms/vk/device.rs` — query
      `PhysicalDeviceVulkan13Features::shader_demote_to_helper_invocation`,
      enable it on the logical-device create when supported, and
      log a warning + fall back to a non-demoting shader variant
      when not. User has a WIP stash with this work already in
      progress on `feature/frame-builder-submit-rate`.

## WM-specific behaviour

- [ ] **e16 popup rounded corners.** Cosmetic — popup outer shape is
      Set+Intersect rectangular bounding; rounded look comes from
      bg-pixmap content, with small black pixels at the very corners
      because the popup outer's bg isn't auto-filled there
      (default bg = None). Investigate `ParentRelative` bg forwarding.
- [ ] **openbox frame chrome.** Frame title bars / labels / buttons
      don't draw under openbox even after the Phase 3.4 atom fix;
      clients render correctly inside the frames. Suspect: openbox
      draws frame decorations into 1×1 sub-windows of the frame and
      that drawing path doesn't reach the host (same family as the
      old wmaker chrome bugs).

## Window lifecycle

- [ ] **Top-level window vanishes mid-session while the X client
      process stays alive.** First observed 2026-05-30 ~00:00 local on
      bee/yserver-hw under MATE: an alacritty terminal disappeared
      from screen while `ps auxf` still showed the alacritty
      process, and a `DumpDrawables` dump confirmed the window was
      gone from yserver's tree (not just unmapped — absent
      entirely). Marco kept compositing fine (the cat wallpaper +
      a newly-spawned terminal rendered to the scanout), so this
      is a per-window teardown, not a compositor stall. The bug
      was not triggered by deliberate user action — happened
      spontaneously during a long-running `cargo build`.
      Root cause **unknown** — the active log was `RUST_LOG=warn`
      (`Justfile:469` at the time) and produced no warn-level
      trace: no `BadWindow`, no `BadDrawable`, no destroy/
      disconnect lines. The only client-level warn in the window
      was 9× `client 14 CreatePixmap BadIDChoice pid=0xffffffff
      (out-of-range)` at 22:06:31 UTC, but the timing analysis put
      alacritty's vanish before the first warn line (21:56:47, a
      VT-switch-recovery EACCES burst), so client 14 is almost
      certainly the debug terminal spawned after the bug — not
      alacritty.
      Captured artefacts (saved 2026-05-30 ~00:09 local):
      `yserver-hw-mate.log`, `yserver-v2-drawable-0-windows.txt`,
      `yserver-v2-scanout-0-out0.ppm`, the 32 `present-src-*.ppm`
      ring dumps, and `scanout-0.png`.
      Next-time setup: `Justfile:466` now defaults the
      `yserver-mate-hw` recipe's `RUST_LOG` to include
      `yserver_core::core_loop::pointer_fanout=trace,
      yserver_core::core_loop::process_request=debug` — useful but
      doesn't cover client lifecycle. For the next repro also
      raise `yserver_core::core_loop::process_disconnect=debug`
      (currently logs `process_disconnect: client X close_mode=Y`
      at debug level — `process_disconnect.rs:92`) and the
      `client_reader` module so the EOF / I/O-error path that
      drops a client's fd is visible. Cheapest follow-up if a
      grep-level trace is enough: promote
      `process_disconnect::process_disconnect`'s entry log
      from `debug!` to `warn!` so future warn-only captures pin
      the disconnect timing.
      Open hypothesis space (none yet refuted):
      a) alacritty's X connection broke (e.g. EPIPE on a write)
         → yserver fired `Message::ClientDisconnected` →
         `process_disconnect` destroyed its windows; alacritty
         survived because its main loop doesn't exit on X EOF.
      b) marco issued `KillClient` or `DestroyWindow` against
         alacritty for reasons unknown.
      c) alacritty itself sent `DestroyWindow` from a recovery
         path triggered by an earlier (silent) error.
- [ ] **BadWindow storm on screensaver/dialog teardown.** Observed
      2026-05-31 on bee/MATE during a clean screensaver
      activate → blank → wake → unlock cycle: ~10–15 `BadWindow
      (code=3)` errors land on marco (client 11) and a secondary
      MATE component (client 14) within ~2s of each saver-window
      DestroyWindow. The bad xids (`0x3f00004`, `0x1000129`,
      `0x10000e`) are the just-destroyed saver/dialog windows;
      the failing requests are GetProperty (opcode 20),
      GetGeometry (14), GetWindowAttributes (3), ChangeProperty
      (18), ReparentWindow (7), CirculateWindow (19). marco/MATE
      have requests queued against the window between yserver's
      DestroyNotify dispatch and the clients draining their
      stream. Cosmetic in logs and functionally invisible (the
      X spec requires the server to error in this case and the
      client to handle the error), but the volume is high
      enough to suggest yserver could either delay DestroyNotify
      until clients ACK their queues (Xorg doesn't), or recognise
      the racey pattern and demote to a single rollup line.
      Lower priority: real Xorg has the same race, just with
      different timing characteristics that hide it most of the
      time. No fix required unless a future investigation finds
      a downstream effect.

## Extension polish

- [ ] **Real RANDR `SetCrtcConfig` / `SetScreenConfig` (runtime
      mode change).** The stub above always replies Success
      without actually switching modes. To honour the client's
      request we'd need ~2–3 phases of work spanning the protocol
      and KMS backend:
      1. Enumerate all KMS-supported modes per connector via
         `drmModeGetConnector` and surface them through
         `GetScreenResources` / `GetOutputInfo` with stable XIDs.
      2. Round-trip mode XID → `drmModeModeInfo` for atomic
         commit.
      3. Runtime modeset entry point in `kms::backend` (we have
         startup `enable_output` + `disable_output`; need a
         "switch this CRTC to mode M" that re-allocates scanout
         BOs at the new size, reprograms the atomic commit, and
         rolls back on kernel rejection).
      4. Resize composite pass attachments + Vulkan framebuffers;
         drain in-flight work cleanly across the switch.
      5. Resize the X11 root, fire ConfigureNotify/ResizeRequest
         to top-levels, recompute the window-tree layout against
         the new screen extent.
      6. Send `RRCrtcChangeNotify` + `RRScreenChangeNotify` to
         every RANDR client that selected input (depends on the
         separate "RRSelectInput mask storage" followup
         landing first).
      7. Multi-output sequencing + partial-failure semantics when
         the client configures multiple CRTCs in one request flow.
      8. Wire-level validation: reject mode XIDs not ours, reject
         kernel-rejected configurations.
      Each piece is reasonable on its own; together it touches the
      compositor critical path and is its own phase of work.
      Deferred until the use case (interactive resolution change /
      hot-plug reconfiguration) becomes load-bearing.
- [ ] **e16 RENDER coverage audit.** Was deferred in Phase 3.4 because
      e16 didn't reach a stable rendering state. Phase 3.4's atom-name
      fix unblocked e16 startup, so this audit is now actionable. Run
      e16 for ~60s, capture all RENDER opcodes touched, compare against
      the implemented set in status.md's RENDER section.
- [ ] **xclock "Missing charsets in String to FontSet conversion"
      warning at startup.** Benign and matches stock Xorg behaviour
      without CJK bitmap fonts installed — libXt walks the locale's
      full charset wishlist (jisx*/gb2312*/ksc*/big5*/various
      iso8859-N) and warns for those it can't load, then proceeds
      with whatever it found (iso8859-1 + iso10646-1). Fix would be
      to probe each font's `FcCharSet` in `build_font_catalog` and
      emit jisx*/gb2312*/ksc*/big5 entries when a real CJK font is
      installed. Defer until a CJK rendering client matters.
- [ ] **Chromium/ANGLE GL init fails: "failed to create drawable" /
      "Could not create the initialization pbuffer" (2026-06-03,
      silence, Mesa libgallium-26.1.1).** Chrome's GPU process can't
      initialize GL on yserver; it worked 2026-05-29 (status.md dogfood),
      broke after a rolling-Arch Mesa+chromium update — **not a yserver
      regression** (see memory `project_chromium_glx_pbuffer_gap`). Two
      GLX gaps were found and FIXED, merged to master
      (`225037a` server-string extension list + `082a5b5`
      GetDrawableAttributes geometry / pbuffer parse), both verified
      byte-for-byte against an Xorg xtrace of the same launch. Those advance Chrome past the GLX-attribute stage but it
      **still fails further in**, now client-side in Mesa's DRI3 drawable
      creation. Boundary established with the Xorg-vs-yserver diff +
      `tools/chromium-yserver-debug.sh`: yserver is correct at *every*
      observable point — GLX extensions/attribs match Xorg, DRI3 version
      1.2 matches, DRI3::Open hands out the right render node (radeonsi
      RX580, `pci id 1002:67df`), the 1×1 init buffer imports with no
      error, and all DRI3 opcodes are implemented. Mesa prints
      `Using DRI3 for screen 0` then `failed to create drawable` with
      **no further server round-trip**, and Xorg's Mesa does the
      identical sequence successfully. Prime suspect: yserver
      **synthesizes** GLX FBConfigs (`synthesise_glx_fb_configs`) rather
      than mirroring radeonsi's real `__DRIconfig`s, so a config Mesa-26
      picks may not map to a driver config at `createNewDrawable`. Next
      step needs Mesa-level debugging (debug Mesa build, or
      `strace -f -e ioctl,openat` on the GPU-process render-node fd) — X
      protocol analysis is exhausted. Low priority if Chrome runs via
      SwiftShader fallback; revisit if HW GL in Chrome becomes
      load-bearing. Repro: `tools/chromium-yserver-debug.sh` from a
      terminal on the yserver session.
## Core loop fairness

- [~] **Listener accept starves under high-volume per-client request
      streams.** Not currently reproducible (as of 2026-05-15) but
      the unbounded per-iteration read budget that underlies it is
      still present, so the shape remains. Original observation
      from 2026-05-13 with `just yserver-xfce-hw`:
      xfce4-panel made 5 attempts to open `:7.0` over ~50ms, all
      returned "cannot open display"; yserver-hw.log shows zero new
      client setups between 13:00:48 (client 38) and the user's
      Ctrl-Alt-Backspace zap at 13:00:59, despite the kernel-side
      `connect()` succeeding for those 5 panel attempts.
      Meanwhile client 13 (likely xfdesktop or xfsettingsd) sent
      **32,293 QueryPointer requests in 11 seconds** (~3000/sec) and
      client 6 (xfwm4) was hammering SHAPE::Combine. The single-threaded
      core loop's per-iteration work was dominated by reading existing
      clients' floods; mio readiness for the LISTENER_TOKEN was queued
      but never serviced, so the new fds languished without an X11
      setup handshake completing.
      Root cause: `core_loop::run_core`'s mio poll iteration likely
      drains each ready fd until WouldBlock before checking the next
      one (a classic head-of-line blocking pattern). Fix shape: cap
      per-client read budget per poll iteration (e.g., 16-32 requests
      per client per tick) so a chatty client doesn't monopolize the
      core. Alternatively, prioritize LISTENER_TOKEN at the top of the
      iteration. Touches `core_loop/run.rs`; medium scope.
      Secondary observation: client 13's 3000 Hz QueryPointer polling
      is itself worth investigating — that's likely yserver returning
      a stale or wrong reply to xfdesktop's pointer query, causing it
      to retry indefinitely. Spec compliance issue.

## Dev-loop / observability

- [ ] **README pointer to this file.** So the next reader knows
      where the bug ticklist lives.
- [ ] **yserver stalls under a headless virtio-gpu with no display
      (`-display none`).** Surfaced 2026-05-26 while bringing up xts
      `Xlib9` in vng: the `xts-yserver` recipe used
      `-device virtio-gpu-pci -display none`, under which KMS atomic
      pageflips never get a completion event. yserver's compose path
      gates on flip-pending state (Stage 2f per-output gate), so with
      no flip-done the scene loop intermittently wedges — clients
      drawing to windows hang mid-test. Symptom in xts was a
      deterministic stall a few test-cases in, then `Could not open
      display :7` ABORTs for every later case (the wedged compose
      starves new-client setup). Switching the recipe to the Venus
      `egl-headless,gl=on` display config (a working flip path)
      removed it: full `Xlib9` went from 4 cases + 1375 ABORT to 18
      cases + 0 ABORT in the same time budget. The recipe is fixed;
      the underlying robustness gap remains — yserver should not
      livelock when pageflip completions never arrive. A
      pageflip-timeout / degraded-present path in the compose loop
      would harden the headless / broken-KMS case. Was repeatedly
      misread as a "depth-4 GetImage hang" because the depth-4
      `UnsupportedDepth` warnings were the last thing logged before
      each stall; the depth-4 path itself is fast (verified offscreen
      under lavapipe) and unrelated.

## Archived: ynest-era (pre-rendering-rework, KMS-direct supersedes)

yserver now runs KMS-direct (no host X server). The items below
were filed when yserver ran nested under `ynest` against the
host Xorg; many of the failure modes simply don't exist on the
KMS path. Kept here for reference if we ever revive ynest-based
WM smoke testing.

- [ ] **CopyArea BadMatch (~100/wmaker startup).** ynest passed
      `CopyArea` requests through to the host X server, which
      rejected drawable pairs of mismatching depth. Pre-rework
      noise from wmaker on ynest.
- [ ] **PolyFillRectangle BadMatch (~15/wmaker startup).** Same
      shape as the CopyArea pattern (ynest forwarded; host
      rejected).
- [ ] **Composite NameWindowPixmap BadAccess (~28/wmaker
      startup).** ynest called `NameWindowPixmap` on un-redirected
      windows; host rejected with BadAccess. Mattered for
      picom-like compositors under the old setup.
- [ ] **MIT-SHM `XShmPutImage` host fast path.** Would have let
      ynest forward a single 256-KB+ image to the host without
      chunking. Irrelevant on the KMS path (no host server).
- [ ] **gtk3-demo / gtk4 demo silent-exits.** gtk-demo started
      and exited silently against the host `:0`, nested or not.
      Pre-existing dconf/stdin quirk in the dev environment.
- [ ] **Down-grade known-benign host errors to DEBUG.** Small
      classifier in `BackendEventSink::handle_backend_event` to
      drop the WARN noise from ynest-host error replies. No host
      server on the KMS path.
- [ ] **fvwm3 segfaults on host container resize / window close.**
      Triggered by ynest-host window resize. yserver KMS doesn't
      currently support output resize at all, so this code path is
      not reachable. Pre-existing fvwm3 bug independent of yserver.
- [ ] **fvwm: apps disappear after host container resize.** Same
      ynest-host-resize trigger as the segfault above. Not
      reachable on KMS-direct.
