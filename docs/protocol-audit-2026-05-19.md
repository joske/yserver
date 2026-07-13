# Protocol audit — RENDER / COMPOSITE / XFIXES / DAMAGE vs Xorg

Audit date: 2026-05-19. Four sub-agents each cross-referenced yserver v2 against
`/home/jos/Projects/xserver` (render/, composite/, xfixes/, damageext/ + miext/damage/).
Focus: clipping, redirect routing, damage delivery — the compositor-WM battleground.

**Status legend** (updated 2026-05-19 PM):
- ✅ **Done** — patch landed, commit hash listed
- 🟦 **Skipped** — empirically dead code or not exercised by current workloads; reason noted
- ⏸ **Parked** — intentionally deferred; reason noted (typically waiting on prerequisite work)
- ⬜ **Open** — no patch yet

---

## Tier 1 — likely causing visible bugs now

### 1. ✅ Paint into backing B never fires DamageNotify on window W
**Severity: Bug** — **Fixed (verified on silence via xtrace)**
- yserver: `damage_fanout.rs:167-275`
- Xorg: `damageext.c` + `composite/compwindow.c`

Damage fanout matches by `drawable_id == B`, not by `redirected_target` indirection.
Compositors that `XDamageCreate(window=W)` see zero events for auto-redirected paints →
marco/xfwm4 sit silent after the initial composite. Matches "marco emits 0 DamageNotify".

**Resolution:** the audit's diagnosis turned out to be inaccurate for the post-Stage-4d
codepath — `accumulate_damage_full_to_state` is called with the WINDOW xid from the
protocol layer, not the backing. The codex + c0ae57d chain (Manual-redirect backing
becomes `scene_participating=true`) made damage on B accumulate AND fan out to
W-subscribers correctly. User verified end-to-end on the silence machine that
DamageNotify now fires for the compositor-relevant cases.

### 2. ✅ `render_composite` drops src/mask client clips entirely
**Severity: Bug** — **Fixed in `6464531`**
- yserver: `kms/v2/backend.rs:5780-5879`
- Xorg: `render/mipict.c:316-389` (`miComputeCompositeRegion`)

`resolve_picture_for_render` returns no clip for src or mask; engine only sees `dst_clip`.
xfwm4/muffin shadow blits that scope a source picture with `SetPictureClipRectangles`
paint over the whole dst. Active hypothesis in `status.md:1973-1979`.

**Resolution:** new pure helpers `compute_render_composite_clip` (dst_clip ∩
src_clip-translated-to-dst-space ∩ mask_clip-translated-to-dst-space) and
`picture_client_clip` (extracts a Drawable picture's stored clip); wired into
`render_composite` between the dst-clip-shift and the engine call. 8 unit tests with
vectors hand-traced from Xorg's `miClipPictureSrc`. Rendercheck `scoords` / `mcoords` /
`dcoords` / `tscoords` / `tmcoords` all pass post-fix.

### 3. ✅ Scene Manual-redirect subtree prune drops Automatic-redirected descendants
**Severity: Likely-bug** — **Fixed in `6ffd370`**
- yserver: `kms/v2/scene.rs:1296-1556`

If any ancestor has `scene_participating=false`, entire subtree pruned regardless of
per-descendant mode. Xorg only stops normal-scene compositing at the Manual ancestor —
Automatic descendants still get auto-composited into the Manual ancestor's backing.
Symptom: `RedirectWindow(frame, Manual)` + `RedirectSubwindows(frame, Automatic)`
(GTK/marco CSD pattern) makes Automatic widgets vanish. Matches Control Center
missing-menu/widget reports.

**Resolution:** replaced the unconditional `prune_subtree=true` with a per-window emit
gate `paint_target_is_self = has_own_redirected_target || (scene_participating &&
!under_redirected_ancestor)`. Recursion threads `under_redirected_ancestor: bool` —
set true whenever traversal enters a window with its own `redirected_target` (mirrors
`resolve_paint_target`'s stop condition). Non-redirected descendants of redirected
ancestors skip emit (their paint already lives in the ancestor's B); Auto-redirected
descendants under a Manual ancestor emit their own B. Updated codex's existing prune
test to use the realistic state with a backing; new test
`build_scene_emits_automatic_descendant_under_manual_ancestor` pins the audit case.
No visible regression on MATE/XFCE smoke (the audit case wasn't being hit by current
workloads — patch is correctness/spec-compliance only).

### 4. ✅ `pict_format` stored but ignored — xRGB32 on depth-32 storage treated as ARGB32
**Severity: Spec-deviation (active bug)** — **Fixed in `22223f5`**
- yserver: `kms/core.rs:488` (comment said "Currently instrumentation-only")

Compositor offscreens with alpha=0-padding leak transparent black onto wallpaper.
Active 4d shadow/CSD ARGB-vs-xRGB mismatch. Real fix is Stage 4e PictFormat tracking.

**Resolution:** added `RENDER_FMT_XRGB32` (id=5, depth=32, alpha_mask=0) — mirrors
Xorg's `PICT_x8r8g8b8`; `write_render_query_pict_formats_reply` now advertises 5
formats. XRGB32 remains available for explicit Picture creation, while the depth-32
ARGB visual is associated only with ARGB32, matching Xorg's one-format-per-visual
screen mapping. New engine
helpers `resolve_force_opaque_pict_format` / `swizzle_class_for_pict_format` /
`dst_has_alpha_for_pict_format` consult `pict_format` first, falling back to the
legacy depth heuristic when `pict_format=0` (engine-internal callers without Picture
context). `render_composite` and `render_traps_or_tris` both thread
src/mask/dst pict_format from the backend's `picture_pict_format()` lookup. 4 new
unit tests pin the helpers across all format quadrants. **Caveat:** the audit's
specific failure mode wasn't observable on current workloads because the server didn't
*advertise* XRGB32 before this patch — so no client could request it. Future
compositors / Cairo paths that opt into XRGB32 now get correct sampling +
pipeline + readback selection. Codex review caught dst-side + trap/tri-path gaps on
successive iterations; both fixed in-patch.

### 5. 🟦 `render_composite_glyphs` silently drops everything except `Over` + SolidFill source
**Severity: Bug** — **Skipped (empirically dead code)**
- yserver: `kms/v2/backend.rs:5944-5947`
- Xorg: supports full op table for CompositeGlyphs

No client error, just counter bump. Cairo gradient-source glyph paint (mate Control
Center yellow headers, GTK header-bar text on a gradient background) renders nothing.

**Empirical cross-check:** 100% of CompositeGlyphs in both `mate.xtrace` (416 calls)
and `xfce.xtrace` (109 calls) use `op=Over(0x03)` with SolidFill source pictures.
The "yellow headers" symptom from the audit doc was a guess at why headers might
fail; user confirmed MATE Control Center renders correctly under current workloads.
The dropped paths in this code are dead for what we ship through. Revisit if a
workload appears that exercises non-Over ops or non-SolidFill sources (e.g.,
gradient-textured text via Cairo `set_source(gradient) + show_text`).

### 6. ✅ Seed copy on redirect activation reads from W, not from parent
**Severity: Likely-bug** — **Fixed in `6f86d7f`**
- yserver: `kms/v2/backend.rs:849-953`
- Xorg: `composite/compalloc.c:541-606` (`compNewPixmap` copies parent w/ IncludeInferiors)

Manual-redirected freshly-created W seeds B with default-init opaque black even when
parent shows real pixels. Matches recurring "black band on map".

**Resolution:** new `seed_backing_from_parent` walks `resolve_paint_target` on
W's parent and `copy_area`s parent-storage-at-W's-position into B's (0, 0). For
chained-redirect parents this routes through the parent's own backing (its
visible storage) rather than the parent's empty own-storage. Three obsolete
Vk-ignored tests that pinned the W-seed model deleted; new
`v2_redirect_seed_uses_parent_content_at_w_position` pins the new behaviour
with a red-parent / default-init-W vector. Net -213 lines.
**Known deferral:** strict IncludeInferiors (parent's *other* children
contributing to the seed where they overlap W's screen position) NOT
implemented — yserver's parent storage already includes most sibling content
via normal paint flow for non-compositor cases, and Stage 4d's scene walk
presents siblings directly. Revisit if a compositor workload surfaces a
sibling-bleed-through case.

### 7. ✅ CreateRegionFromGC translates by clip_origin; Xorg does not
**Severity: Bug** — **Fixed in `1ae3d87`**
- yserver: `process_request.rs:2622-2626`
- Xorg: `xfixes/region.c:204-232` — returns raw clip-coordinate rects, origin only
  applied when GC is *used*

A WM that builds a region from a GC then re-installs as that GC's clip gets
double-translation → wrong area shadowed/repainted.

**Resolution:** dropped the `nested::offset_rects(rects,
clip.x_origin, clip.y_origin)` wrap at the CREATE_REGION_FROM_GC
arm; the region now stores the raw rects from
`xfixes::parse_rectangles`, mirroring Xorg's `XFixesRegionCopy(pGC->clientClip)`.
`nested::offset_rects` stays in tree — it's still used by SHAPE /
XFIXES translate-region paths where the offset IS part of the
operation's intent. New TDD test
`create_region_from_gc_copies_clip_rects_without_origin_translation`
pins the raw-copy invariant with a non-zero clip_origin vector.

### 8. ✅ `drawable_origin` field defined but never written — mid-WIP
**Severity: Bug (incomplete)** — **Fixed in `8c5c841`**
- yserver: `kms/core.rs:487`, `process_request.rs:1153`
- Backend method `set_picture_drawable_origin` had a trait default no-op
- Also: `picture_client_clip_rects` had a trait default `None`

Window-backed pictures whose drawable is at non-(0,0) (CSD frame children) have clips
in dst-coords, get clamped by `dst_extent.max(0)`, lose negative-shifted rects.
**This is the in-flight work referenced by the current diagnostics.**

**Resolution:** v2 now overrides both trait methods. `set_picture_drawable_origin`
writes into `PictureRecord::Drawable.drawable_origin` (no-op tolerated on SolidFill /
gradient variants). `picture_client_clip_rects` returns the standard Some(rects) /
Some(None) / None tristate per Xorg semantics so `CreateRegionFromPicture` works
end-to-end. 6 unit tests (5 mine + 1 codex review addition pinning that
drawable_origin must NOT be folded into the returned clip rects). The
`drawable_origin` field is persisted but has no live consumer in the engine yet —
that wiring stays a known follow-up; this commit closes the WIP-completion the audit
flagged.

---

## Tier 2 — will hang or visibly break clients

### 9. ✅ `SelectSelectionInput` never emits `XFixesSelectionNotify`
**Severity: Bug** — **Fixed in `5e19e33` + 5 code-review follow-ups**
- yserver: `process_request.rs:2555-2568`; event emit site missing entirely
- Xorg: `xfixes/select.c:53-94`, `dix/selection.c:126-211`

Every clipboard manager (klipper, copyq, gpaste, gnome-shell clipboard indicator) wedges.

**Resolution:** all three notify subtypes wired — `SetSelectionOwner`
from `handle_set_selection_owner`, `SelectionWindowDestroy` from
`destroy_window_subtree` (shared helper
`drop_selections_owned_by_windows` matches selections whose owner
window is in the destroyed subtree), `SelectionClientClose` from
`process_disconnect` (before the owned-window destroy loop so
`state.resources.window_owner` still resolves). New wire encoder
`encode_selection_notify_event` + `SELECTION_NOTIFY_*` /
`SELECTION_MASK_*` / `SELECTION_ALL_EVENTS_MASK` constants in
yserver-protocol.

Code-review iterations landed on top of the initial patch — each
verified directly against the named Xorg line range:

- `7d2508a` — order + timestamps on destroy/close: notify BEFORE
  clearing `state.selections` (matches `dix/selection.c:131-138`'s
  `DeleteWindowFromAnySelections` order); track per-selection
  `lastTimeChanged` (extends `state.selections` value to
  `(ResourceId, u32)`); event payload carries currentTime +
  prior lastTimeChanged per `xfixes/select.c:88-89`.
- `9f8ef05` — drop the `max(1)` clamp on currentTime; switch to
  existing `ServerState::timestamp_now()`. Xorg's
  `UpdateCurrentTimeIf` (`dix/dispatch.c:226-236`) has no floor.
- `f176d32` — `SelectSelectionInput` window + mask validation per
  `xfixes/select.c:189-196` (BadWindow / BadValue).
- `0760f9c` — `SetSelectionOwner` timestamp rules per
  `dix/selection.c:164-211`: future-shifted requests silently
  ignored; `ClientTimeToServerTime` (time=0 → currentTime); stale
  requests vs prior `lastTimeChanged` silently ignored;
  notify carries currentTime + the just-stored lastTimeChanged.
- `abbadbf` — `SetSelectionOwner` BadWindow + BadAtom validation
  in Xorg's exact order (future → BadWindow → BadAtom → stale →
  update; `dix/selection.c:164-178`).
- `e073fc7` — SelectionClear gate matches
  `dix/selection.c:194`'s prior-owner-client identity check (NOT
  window identity); event time field carries resolved server time,
  not raw `time_val`; BadWindow/BadAtom error replies carry major
  opcode 22 (was 0 → malformed).

Test count climbed to 13 (3 encoder in yserver-protocol; 10 fanout
+ validation + timing in yserver-core).

**Real-session verification (yoga, 2026-05-20):** parcellite under
mate-session picks up selection changes from xclip / wezterm /
applications immediately — the symptom the audit named ("every
clipboard manager wedges") confirmed resolved end-to-end.

### 10. `GetClientDisconnectMode` (XFIXES minor 34) has a reply but is silently dropped
**Severity: Bug**
- yserver: `_` arm in XFIXES dispatch
- Xorg: `xfixes/disconnect.c:89-109`

docstring claimed "no block" — wrong. gnome-session probe hangs to read timeout.

### 11. Map-window doesn't emit damage (server background paint not hooked)
**Severity: Bug**
- yserver: `process_request.rs:8610` — calls `backend.map_subwindow` but no damage bump
- Xorg: `miext/damage/damage.c` hooks `miPaintWindow`

Tooltip/menu first-frame missing on marco compositing.

### 12. `DamageSubtract` doesn't re-fire when damage still non-empty after subtract
**Severity: Bug**
- yserver: `process_request.rs:4168-4170`
- Xorg: `damageext.c:422-424` — calls `DamageExtReport` again if still non-empty

RepeatNotify mechanism dead. Compositor stalls if another client adds damage while
the focused client is between Subtracts.

### 13. Raw/Delta DamageNotify throttled to one event per Subtract cycle
**Severity: Bug**
- yserver: `damage_fanout.rs:241,271` — `if !fired { … fired = true }`
- Xorg: `damage.c:1941-1953` — emits every paint for Raw; emits on new area for Delta

picom (uses DeltaRegion) misses incremental damage between two Subtracts → smearing.

### 14. ⏳ `SelectCursorInput` never emits `XFixesCursorNotify`; `GetCursorImage` returns real data
**Severity: Bug** — `GetCursorImage` unblocked 2026-05-19; `XFixesCursorNotify` event emission still TODO
- yserver: `process_request.rs:2607-2632` (real reply path now sources from
  `Backend::get_active_cursor_image`); `xfixes.rs:347-410`
  (`encode_get_cursor_image_reply` premultiplies straight BGRA → wire ARGB)
- Xorg: `xfixes/cursor.c:360-413`

**Status (2026-05-19, post Stage 5a-c):** `GetCursorImage` now returns the
currently-effective cursor's pixels + position + hotspot + monotonic serial,
sourced from `KmsBackendV2.cursor_records[effective_cursor_xid]` (v2) — the
records landed in Stage 5 Phase A. Pre-Stage-5 backends (`ynest`,
`RecordingBackend`) keep the empty-reply fallback (`get_active_cursor_image`
default is `None`). Screencast/magnifier readers see the correct cursor image.

**Remaining**: `XFixesCursorNotify` event emission when the effective cursor
changes — backend → core event routing not wired yet; mask storage at
`process_request.rs:2597-2606` is in place, the emit-side is the gap. Likely
~30 LoC once a `Backend::pop_pending_xfixes_events` (or similar) shape is
agreed.

### 15. `ExpandRegion` (XFIXES 3.0, minor 28) stubbed
**Severity: Stub**
- yserver: falls through `other` arm
- Xorg: `xfixes/region.c:732-770`

xfwm4 uses ExpandRegion for shadow extents → shadows visually undersized.

### 16. `CreateRegionFromBitmap` returns empty rects
**Severity: Bug**
- yserver: `process_request.rs:2598-2643` — inserts `Vec::new()`
- `bitmap_to_yx_banded_rects` exists at `nested.rs:736` but not wired up

xfwm4/picom pixel-precise shadow shapes break; SetWindowShape with bitmap clears shape.

---

## Tier 3 — spec correctness, lower priority

### COMPOSITE

- **NameWindowPixmap missing `viewable` check** — should be BadMatch for unmapped W
  (`process_request.rs:3185-3220` vs `composite/compext.c:241-242`)
- **NameWindowPixmap accepts InputOnly windows** — should be BadMatch
  (`process_request.rs:3175-3280` vs `compext.c:250-252`)
- **NameWindowPixmap missing `LEGAL_NEW_RESOURCE` precedence** — wrong error class on
  duplicate XID
- **Mode-flip Manual→Automatic emits no synthetic damage** — compositor misses repaint
  trigger (`process_request.rs:615-662` vs `compalloc.c:299-307`)
- **`compReallocPixmap` not short-circuiting on size-unchanged moves** — every drag
  invalidates all NameWindowPixmap aliases, causing flicker
  (`process_request.rs:781-874` vs `compalloc.c:698-708`)
- **COW born without `MapNotify` fanout** — marco's StructureNotify subscription never
  fires (`kms/v2/backend.rs:3925-3983`)
- **COW has no input-passthrough region by default** — pointer events may be absorbed
  by COW before WM input proxy (Xorg's COW born input-transparent)
- **COW `host_xid: None` pre-GetOverlayWindow** — any client request against 0x103
  before compositor issues GetOverlayWindow bypasses normal BadWindow/BadDrawable
- **ConfigureWindow on Manual-redirected child before first map emits no damage**
  (`process_request.rs:7508-7512` — gated on `redirected_backing.is_some()`)
- **`composite_named_pixmaps` aliases never disposed** — memory growth across
  compositor restarts; see status.md Task 4 (resources.rs vs process_disconnect.rs)

### RENDER

- **`clip_mask = Pixmap` on ChangePicture is a logged no-op** (`backend.rs:2205-2218`)
  — Cairo subpixel-mask path and shaped-window paths broken
- **`subwindow_mode` (IncludeInferiors vs ClipByChildren) stored but never read**
  — window-backed pictures always behave as IncludeInferiors; child windows
  aren't clipped out from source sampling
- **Alpha-map (`alpha_map`/`alpha_x`/`alpha_y`) stored but never sampled**
  (`core.rs:507-509`) — alpha-modulated source pictures silently ignore it
- **`SetPictureClipRectangles` v2 site doesn't canonicalize** (`backend.rs:6807-6821`)
  — status.md claims this landed, but code doesn't show it; needs verification (?)
- **Mask picture clip not passed to engine for direct Composite-with-mask-picture path**
  (`backend.rs:5771-5779`)

### XFIXES

- **QueryVersion hard-codes (5,0) instead of `min(client, server)`** — a client
  requesting V1 sees V5 back and may issue V2+ opcodes
  (`process_request.rs:2542-2548`)
- **Combine/Invert/RegionExtents silently ignore missing source** — should return
  BadRegion; instead treats missing as empty
- **CopyRegion/Subtract/Combine don't validate dest exists** — should return BadRegion
  for unknown dest XID
- **CreateRegionFromWindow accepts `kind=Input`** — Xorg returns BadValue
- **FetchRegion silently returns empty for missing region** — should return BadRegion
- **ChangeSaveSet target+map ignored** (`process_request.rs:2849-2851`) — windows that
  should re-root on client disconnect vanish instead

### DAMAGE

- **Ancestor walk ignores `border_width`** — damage coords off by border width in
  parent space for any WM using non-zero borders (`damage_fanout.rs:93,105`)
- **DamageNotify NonEmpty fires even when damage was already non-empty** (should be
  empty→non-empty transition only) (`damage_fanout.rs:252-261`)
- **Configure damage only fires at new position, not old** — old-position trails on
  drag until full repaint (`process_request.rs:7508-7514`)
- **`DamageSubtract` doesn't re-clip to drawable extent post-subtract**
  (`process_request.rs:4141-4170` vs `damage.c:1854-1873`)
- **`DamageAdd` doesn't translate by `pDrawable->x/y`** (`damageext.c:454-456`)
- **No damage cleanup on DestroyWindow / FreePixmap / client disconnect** — dangling
  entries accumulate across compositor restart; phantom notifies on recycled XIDs

---

## Tier 4 — stubs unlikely to trip current clients

- RENDER: `AddGlyphsFromPicture` (minor 21), `Scale` (9), `ColorTrapezoids/Triangles`
  (14/15), `AnimCursor` (31), `AddTraps` (32), `ConicalGradient` (36)
- XFIXES: `GetCursorName`/`GetCursorImageAndName`/`SetCursorName`/`ChangeCursor`
  (minors 23-26), `CreatePointerBarrier`/`DestroyPointerBarrier`/`SetClientDisconnectMode`
  (31-33)
- DAMAGE: `DamageNoteCritical`; `DamageAdd` origin translation; `DamageQueryVersion`
  dispatch gate; all Poly*/FillPoly/PolyText over-damage by full drawable (correct but
  wasteful)
- ~~RENDER `render_create_cursor` allocates xid but never rasterises (themed cursors)~~
  — **Done 2026-05-19** (Stage 5 Phase A `feat(stage-5a)`). v2 now reads the
  Picture's BGRA pixmap via `engine.get_image` and stores it as a
  `CursorRecord`; `define_cursor` swaps the scene's `CursorEntry` to a
  freshly-rasterised sprite. Themed cursors (Cairo / GTK / Qt) work
  end-to-end.

---

## Recommended fix order

For maximum compositor-WM impact with minimum churn:

1. ✅ **#1** Damage delivery via `redirected_target` — `c0ae57d` + `a4309f5` (codex);
   user-verified on silence
2. ✅ **#2** RENDER src/mask clip propagation — `6464531`
3. ✅ **#8** Finish `set_picture_drawable_origin` / `picture_client_clip_rects`
   WIP — `8c5c841`
4. ✅ **#3** Scene subtree prune — `6ffd370`
5. 🟦 **#5** CompositeGlyphs drop non-SolidFill source silently —
   skipped, empirically dead code (0% of marco / xfwm4 CompositeGlyphs use
   non-Over op or non-SolidFill src in captured traces)
6. ✅ **#9** XFixesSelectionNotify — `5e19e33` (set-owner / window-destroy / client-close subtypes)
7. ⬜ **#11 + #12** Map damage + Subtract RepeatNotify — compositor loop completeness
8. ✅ **#4** PictFormat tracking (xRGB vs ARGB) — `22223f5`; ended up the larger
   piece (Stage 4e substrate done in one patch, including dst-side + trap/tri-path
   coverage caught by codex review)

**Tier 1 closed.** All eight items resolved: #1 `c0ae57d` + `a4309f5`,
#2 `6464531`, #3 `6ffd370`, #4 `22223f5`, #5 skipped (empirically
dead code), #6 `6f86d7f`, #7 `1ae3d87`, #8 `8c5c841`.

**Bonus tier-1 found during yoga mate-hw smoke 2026-05-19 PM** (not in the
original audit list): Manual-redirect backing's `scene_participating=false`
silently dropped 96% of `store.damage()` calls on it, leaving redirected
windows invisible except where cursor-projected damage happened to overlap.
Fixed in `3a8e028` (separate from this audit document since it's a v2
internal damage-flow issue, not a protocol-surface vs-Xorg deviation; see
`docs/status.md` Stage 4d follow-ups for the long form).

---

*Generated from parallel agent audit 2026-05-19. Xorg reference: `/home/jos/Projects/xserver`.*
