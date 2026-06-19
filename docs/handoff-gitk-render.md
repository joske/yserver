# Handoff: gitk rendering bugs under Cinnamon/yserver

Date: 2026-06-19. Machine: silence (RX580, gfx8 Polaris, RADV). Started from
lightdm → Cinnamon (muffin compositor). App under test: **gitk** (Tk/Tcl).

Two **distinct** bugs were found. Bug A is fixed (uncommitted, on this branch).
Bug B is **open** and is the reason gitk is still unusable.

Branch: `fix/copyarea-window-source-backing` (cut from `master` @ `a6f20f68`).
Nothing is committed yet — all changes are in the working tree.

---

## Bug A — diff-pane blanks while scrolling — **FIXED (verified on HW)**

### Symptom
Scrolling gitk's diff (text) pane progressively blanked it; the commit-list
(canvas) pane scrolled fine. Clicking/hovering repainted touched bits.

### Root cause
`KmsBackendV2::copy_area` (`crates/yserver/src/kms/v2/backend.rs`) resolved the
**destination** through `resolve_paint_target` (→ the Composite redirect
backing) but read the **source** from the raw `store.lookup(src_host_xid)`.
For a window that renders into a redirect backing (any window under muffin),
the window's *own* leaf storage is never painted — content lives in the
backing. Tk's text widget scrolls via `XCopyArea(src=win, dst=win)`; that
self-copy read the blank leaf storage and wrote blank into the backing.

Confirmed visually with a SIGUSR2 drawable dump (Ctrl-Alt-F12): the diff text
widget's own storage (`win-0x…`) was blank white; gitk's backing diff region
was blank; scanout showed grey gitk.

### Fix (the keep-able part of the diff)
In `copy_area`, resolve a **window** source through `resolve_paint_target` too
(same as the destination), applying the source's backing offset (`src_off`) to
the source coordinates. **Pixmap** sources keep the raw lookup (the marco CC
offscreen-pixmap → COW path depends on it). The fix touches:
- the src resolution block (replaces the old `store.lookup` + comment),
- `src_off` added in 3 source-coordinate computations: the `ClipState::Pixmap`
  path, the GC-function/plane-mask CPU path, and the main GPU-blit path.

Verified: user confirmed the diff pane scrolls cleanly after installing.

### NOT yet done for Bug A
- Strip the temp diagnostics (see below).
- Add a regression test (intended: lavapipe `#[ignore]` integration test — a
  self-`CopyArea` on a redirected window preserves content, not blanks).
- Commit + PR. **Deferred**: gitk is still broken by Bug B, so this was left
  unmerged ("no point merging half a fix").

---

## Bug B — whole gitk window intermittently goes grey — **CRACKED BY CODEX 2026-06-19**

> **Resolved** (HW revalidation pending). It was a 3-bug cluster, all in v2 —
> canonical writeup in `docs/status.md`:
> 1. `resolve_paint_target` keeps walking the hierarchy when a descendant has
>    temporarily lost its xid→DrawableId mapping → paint routes to the nearest
>    redirected ancestor backing.
> 2. `render_free_picture` releases the exact `DrawableId` retained at
>    `CreatePicture` instead of re-looking-up by host xid after a window storage
>    rebind (the Drawable-backed RENDER Picture lifetime bug = the
>    `CompositeGlyphs` grey this section predicted).
> 3. `copy_area` subtracts mapped higher siblings that resolve to the same
>    shared backing before dispatch (Tk repaints a lower sibling and copies its
>    full rect over higher panes). Regression
>    `copy_area_into_lower_sibling_excludes_higher_sibling_in_shared_backing`.
>
> The original analysis below correctly fingered the RENDER `CompositeGlyphs` /
> Picture-on-pixmap path; kept for the ruled-out list and evidence artifacts.

### Symptom
Intermittently (various triggers: scrolling, hover, focus changes; "works at
first, then suddenly almost everything goes grey"). The window's **redirect
backing fills to the frame background grey**; only freshly-drawn widgets
(title bar, a button, a dropdown) survive. Clicking repaints some bits. It
**pre-dates Bug A's fix** (happened on the clean binary too), so it is a
separate, independent bug, not a regression from Bug A.

### What is RULED OUT (with instrumentation, not guesswork)
1. **Silent copy-drop** (`host_drawable_target(src)`=None → whole copy skipped):
   instrumented in `handle_copy_area` (`CopyArea-DIAG src-unresolved …`). Never
   fired during a confirmed blank.
2. **`avail=None` background-fill** (source seen as zero-sized → dst filled with
   window background): instrumented (`CopyArea-DIAG avail=None …`). Never fired.
3. **Pixmap-lifetime race** (ephemeral source pixmap freed by `destroy_now`
   while an open frame's deferred copy still references it): instrumented
   `store_decref_with_invalidate` + `engine.open_frame_references`
   (`PIXMAP-LIFETIME-DIAG …`). Never fired. (decref already defers via the
   render-ticket; the protection holds.)
4. **Spin loop / event storm**: the heavy `CreateGC`/`FreeGC`/`GetInputFocus`
   churn is **normal Tk** — the Xorg xtrace shows *more* of it. Not pathological.
5. **Protocol divergence / errors**: xtrace of gitk on yserver vs Xorg shows
   gitk issues the **same requests** and receives **zero errors** on both.

### What is ESTABLISHED
- gitk renders **text via RENDER `CompositeGlyphs`** into Pictures (Xft), **not**
  core text ops (zero `ImageText`/`PolyText`). This is why grepping for
  `drawable=<pixmap>` draws between a pixmap's fill and its copy found nothing —
  the glyphs go to a **Picture** on that pixmap, not the drawable directly.
- gitk's per-pane pattern: `CreatePixmap` → `CreatePicture` on it →
  `PolyFillRectangle` (grey background) → `CompositeGlyphs` (text into the
  picture) → `CopyArea` pixmap→window → `FreePixmap`. Heavy churn (≈386
  CreatePixmap/frame-ish).
- yserver **accepts everything and silently mis-renders**: the backing ends up
  with only the grey background fill, missing the composited glyphs/content.

### Leading hypothesis (unproven)
The RENDER `CompositeGlyphs` → Picture-on-pixmap result is not present in the
pixmap's storage at `CopyArea` time, so the copy carries only the grey
background. Candidates to investigate:
- Picture↔pixmap binding / does `CompositeGlyphs` into a pixmap-backed Picture
  land in the same VkImage the subsequent `CopyArea` reads?
- Intra-frame ordering between the glyph composite and the deferred copy in the
  frame_builder.
- Glyph upload (`AddGlyphs`) / glyphset state intermittency ("works at first").

### Suggested next step
The proven method here (cf. the chromium investigations) is an **xtrace
content-diff vs Xorg**, but the request streams already match — so the next cut
is **inside yserver's RENDER path**: instrument/verify that `CompositeGlyphs`
into a pixmap-backed Picture actually writes the pixmap's storage, then that the
`CopyArea` reads it. A drawable dump (Ctrl-Alt-F12) taken mid-blank that
captures a *source pixmap* (not just the backing) would show whether the glyphs
ever landed in it.

---

## Branch state / what to disentangle before committing Bug A

Working tree (uncommitted) on `fix/copyarea-window-source-backing`:

| File | Contents | Keep? |
|---|---|---|
| `backend.rs` `copy_area` | **Bug A fix** (src resolution + `src_off` ×3) | **KEEP** |
| `backend.rs` `store_decref_with_invalidate` | `PIXMAP-LIFETIME-DIAG` warn | strip (temp) |
| `engine.rs` `open_frame_references` | diagnostic helper | strip (temp) |
| `process_request.rs` | two `CopyArea-DIAG` warn blocks | strip (temp) |
| `Justfile` | `yserver-cinnamon-hw` recipe trace tweak (`process_request=debug,paint=trace`) | user's tracing tweak — revert before commit |

To bank Bug A: strip the three temp diagnostics, revert the Justfile tweak, add
the regression test, commit + PR.

---

## Evidence artifacts (on silence, in repo root unless noted)

- `cinnamon.xtrace` (13M) — gitk on **yserver**; gitk = connection `061`.
- `cinnamon-xorg.xtrace` (114M) — gitk on **Xorg**; gitk = connection `058`.
  (Find gitk by the `-misc-fixed-…jisx0208…` font queries.)
- `yserver-hw-cinnamon.log` — yserver debug+paint trace. NB: backend
  `copy_area` traces use **host** xids (`0x40xxxx`), while `process_request`
  logs **client** xids (`0x1f0xxxx`) — they are the same ops under different
  namespaces (this tripped up the investigation once).
- SIGUSR2 drawable dumps (`yserver-v2-drawable-*.ppm`, `-scanout-*`, `-cow-*`)
  were copied into the repo root during the session; convert with
  `magick file.ppm out.png`. The grey gitk backing + blank source-window
  storage that proved Bug A came from these.

## Gotchas worth remembering
- Cinnamon/muffin composites via **GLX texture-from-pixmap**, sampling window
  backings directly — the X11 COW storage is unused (it dumps white; ignore it).
- gitk uses RENDER for text and classic ephemeral-pixmap double-buffering for
  panes — a combination GTK3/Qt5 don't use, which is why **only Tk apps** hit
  these bugs while GTK/Qt render fine.
