# Tech-debt / stub audit — unimplemented & faked protocol surface

**Date:** 2026-06-17 · **Scope:** full codebase sweep (core opcodes 1–127 + all extensions + mechanical markers) · **Method:** 3 parallel handler-reading agents + mechanical grep sweep

Checklist of incomplete / stubbed / faked implementations, prioritized by client impact. Tick as addressed.

## Headline

**No production panic landmines.** All 16 `unimplemented!()` are in `RecordingBackend` (debug/trace backend; `render_opcode()` returns `None` → unreachable in real use). `unreachable!()`/`panic!` sites are guarded by prior validation. Zero `todo!()`. The debt is **stubs, no-ops, and faked data**, not crashes.

Convention being violated throughout: *"no protocol stubs"* — empty/zero replies that satisfy xts but mislead/break real clients (GTK/Qt/Chromium/Mutter) are latent bugs; unavoidable stubs must WARN+TODO.

---

## Tier 1 — Client *hangs* (reply-bearing request → no-reply catch-all → infinite block)

Highest impact: the client blocks forever, not just gets a wrong answer. Root cause: extension dispatchers' catch-all arms `return Ok(Handled)` without writing reply bytes.

**HANG fixed (stopgap), FEATURE still unimplemented.** These now return `BadImplementation` instead of hanging — but that is NOT protocol-correct (Xorg implements all of them and returns real data). Each site is marked `TODO(unimplemented)` in code so a future pass can grep them and replace the stopgap with a real implementation. Don't mistake the error reply for done.

- [x] **XI2 XIGetFocus** (minor 50) — **REAL implementation** (upgraded from stopgap): returns the real per-device focus window, plus `BadDevice` for an invalid device, reusing XI1 GetDeviceFocus's focus state. No longer a `TODO`. Tests `xi_get_focus_returns_real_focus_window`, `xi_get_focus_invalid_device_returns_bad_device`.
- [x] **RANDR CreateMode** (minor 16) — hang→`BadImplementation` stopgap + `TODO(unimplemented)`. Test `randr_create_mode_unimplemented_returns_error_not_hang`.
- [x] **RANDR CreateLease** (minor 45) — hang→`BadImplementation` stopgap + `TODO(unimplemented)`. Test `randr_create_lease_unimplemented_returns_error_not_hang`.
- [x] **RANDR SetPanning** (minor 29) — hang→`BadImplementation` stopgap + `TODO(unimplemented)` (sibling reply-bearing hang the audit missed). Test `randr_set_panning_unimplemented_returns_error_not_hang`.
- [ ] **Real implementations** still owed for the remaining stopgaps: RRCreateMode (custom modes), RRSetPanning (panning), RRCreateLease (DRM lease). Grep `TODO(unimplemented)`. *(XIGetFocus now done for real — see above.)*
- [x] **Fix approach (decided): SURGICAL, not blanket.** Added explicit arms only for *reply-bearing* unimplemented requests (the hang class). VOID unimplemented requests stay silently accepted via the catch-all — Xorg implements those as success, so erroring them would introduce a NEW Xorg-divergence (vs. the "any divergence is our bug" rule); a missing *reply* is the only client-hanging case. RANDR provider-property reply-bearing requests are unreachable (`GetProviders` returns 0 providers) so left to the catch-all.

## Tier 2 — Advertised but can't deliver (cardinal "no protocol stubs" sin)

Capabilities announced in version/extension replies but not implemented:

- [~] **XFIXES pointer barriers** CreatePointerBarrier/DeletePointerBarrier (minor 31/32) + XI2 XIBarrierReleasePointer (minor 61) — **IMPLEMENTED (Phases 1–4 code), pending HW smoke.** Full feature: confinement clamp (port of `Xi/xibarriers.c`), XI2 BarrierHit/BarrierLeave events + one-shot release, grab semantics, resource/XID lifetime, and the KMS input-thread cursor resync so the wall physically holds on bare metal. Spec `docs/superpowers/specs/2026-06-17-xfixes-pointer-barriers-design.md` (3 codex passes), plan `docs/superpowers/plans/2026-06-17-xfixes-pointer-barriers.md` (6 codex passes), implementation (codex) + review (1 defect fixed: released-leave metadata). ~30 unit/integration tests green; **T13 release gate = bee dual-head GNOME smoke (not yet run).** Was: not even defined; advertised v5.0 forced for Mutter while silently no-op.
- [~] **X-Resource v1.2** — **QueryClients + QueryClientResources now REAL** (the two `xrestop` leans on). QueryClients lists connected clients' XID ranges; QueryClientResources returns per-type counts (WINDOW/PIXMAP/GC/FONT/CURSOR/COLORMAP canonical names from Xorg `dix/registry.c`; PICTURE/GLYPHSET get descriptive server-chosen names since Xorg leaves RENDER types unnamed). New encoders `encode_query_clients_reply`, `encode_query_client_resources_reply` + `ResourceTable::resource_counts_by_owner`. Tests: `query_clients_reply_lists_clients`, `query_client_resources_reply_lists_types`, `resource_counts_by_owner_tallies_per_type_and_skips_zero`, `x_resource_query_clients_lists_connected_clients`, `x_resource_query_client_resources_counts_by_type`. The other 3 queries (QueryClientPixmapBytes/Ids/ResourceBytes) still zero-stub + `TODO(unimplemented)` — need per-client byte tallies / PID map yserver doesn't keep.
- [ ] **GLX `GLX_EXT_texture_from_pixmap`** — advertised, but `BindTexImageEXT` (`:9288`) binds and never updates content (`TODO(glx-tfp)`). (Matches parked TFP work / `2026-06-09-glx-tfp-radv-export-rootcause.md`.)
- [ ] **RENDER QueryFilters** (`:1818`) advertises `convolution`/`bilinear` that `SetPictureFilter` (`:1827`)/backend won't apply (only `nearest` honored). (Matches parked render-convolution work.)

## Tier 3 — Silent wrong behavior / broken contracts

- [ ] **GrabServer / UngrabServer** (core 36/37) — `process_request.rs:202–203`, pure no-ops, **zero backing state** (no `server_grab` anywhere). Test asserts it does nothing (`grab_server_is_log_only_no_op:27769`). WM/toolkit server-grab atomicity (menu grabs, screensaver) silently broken. Highest-impact *core* gap.
- [ ] **XKB read-only façade** (`kms/xkb.rs`) — every `Set*` (SetMap `:15896→None`, SetControls/SetNames/SetCompatMap/indicators) silently swallowed; `GetState` (`xkb.rs:786`) all-zero; `GetMap` (`xkb.rs:301`) drops AltGr/secondary groups; `GetNames` (`xkb.rs:651`) hardcoded "evdev+aliases(qwerty)" not RMLVO-derived. Client keyboard reconfig = success-shaped silence.
- [x] **QueryFont / QueryTextExtents** (core 47/48) — FIXED: invalid fontable now → `BadFont` (was `unwrap_or_default()` zeroed reply). Tests `query_font_invalid_fontable_returns_bad_font`, `query_text_extents_invalid_fontable_returns_bad_font`.
- [ ] **SYNC Await / AwaitFence** (`:2947`/`:3144`) — counters/alarms/fences work, but blocking-await never suspends the client stream (self-described "known gap" `:3179`).
- [ ] **XTEST CompareCursor** always `same=true` (`:5934`); **GrabControl** no-op (`:5964`). NOTE (investigated 2026-06-17): CompareCursor is **not a one-line fix** — Xorg `ProcXTestCompareCursor` compares the window's *effective (inherited)* cursor against `pCursor`, where the `cursor` arg has 3 modes (None→null, `XTestCurrentCursor`=1→the live sprite cursor, else→cursor resource w/ BadCursor). Needs effective-cursor resolution + current-sprite-cursor + identity semantics; a naive `window.cursor == arg` would silently diverge.
- [ ] **RecolorCursor** (core 96) — `process_request.rs:204`, accepted-and-dropped no-op.
- [x] **AllocNamedColor / LookupColor** (core 85/92) — FIXED: unknown name now → `BadName` (was hardcoded gray `0xc0c0`). Tests `alloc_named_color_unknown_name_returns_bad_name`, `lookup_color_unknown_name_returns_bad_name`.

## Tier 4 — Faked / partial data (works enough today, fidelity invented)

- [ ] **PRESENT** — always Copy path (no Flip/scanout, `flip_path==false`); `CompleteNotify.ust` hardcoded 0; synthetic per-window MSC; scheduler never drained for flips (`present_scheduler.rs:208` empty-Vec stub; selector inputs hardcoded `:7618`). OK for Mesa WSI now, risky for frame-pacing clients.
- [ ] **XI device topology** — `ListInputDevices`/`XIQueryDevice` report a hardcoded 4-device model w/ fabricated button/axis/range data (`process_request.rs:9570–9573`); only names real; XIQueryDevice fakes pointer pos to screen-center (`:9971`). (Deliberately non-zero per VSCode `ndevices=0` fatal-CHECK history — but class data invented.)
- [x] **RANDR gamma** — **DONE 2026-06-18 (@294ee0cb).** GetCrtcGammaSize/GetCrtcGamma/SetCrtcGamma implemented: legacy `drmModeCrtcSetGamma`, connector-keyed cache, reapply after every `commit_modeset` (modeset/VT-switch/DPMS). HW-verified on silence/RX580 under Cinnamon — redshift warms the screen and survives a VT-switch (closes the spec's open "legacy gamma under active pageflips" risk: no EBUSY/collision observed). Spec + plan each codex-reviewed ×2 (`docs/superpowers/{specs,plans}/2026-06-18-randr-gamma*`).
- [ ] **RANDR output properties absent** — ListOutputProperties/QueryOutputProperty/GetOutputProperty (`:2338/:2346/:2467`) → zero props / BadName; Change/Configure/Delete fall to catch-all. No EDID/Backlight/scaling-mode.
- [ ] **RANDR SetScreenConfig** (`:2601`) — explicit no-op accept (returns success, applies nothing). RandR-1.0 clients believe resolution changed.
- [ ] **GLX configs** synthesised to mirror radeonsi (`:8911/:8930`) — may mismatch other drivers; CREATE_CONTEXT/MAKE_CURRENT/SWAP_BUFFERS bookkeeping-only; QUERY_CONTEXT zero attribs.
- [ ] **MIT-SHM** `shared_pixmaps=false` (`:5013`); `CreatePixmap` (`:5238`) one-time snapshot, not live-shared.
- [ ] **DRI3** multi-plane `PixmapFromBuffers` hard-rejected if `num_buffers!=1` (`:8024`); `BuffersFromPixmap` hardcoded `nfd=1` (`:8200`).
- [ ] **COMPOSITE** redirect record-only on v1 backend (gated on `supports_redirect_activation()`, `:4558/:4576`); CreateRegionFromBorderClip fakes full-drawable rect (`:4727`).
- [ ] **RENDER CreateConicalGradient** (minor 36) — empty `// stub` (`:2063`); picture XID never registered → downstream Composite silently fails lookup. **AddTraps** (minor 32) — damages but never rasterizes (`:2066`).
- [x] **XI1 GetDeviceKeyMapping** (minor 24) — **REAL implementation.** Now reuses the core `GetKeyboardMapping` path (`fetch_merged_keymap`: backend keymap + `ChangeKeyboardMapping` overrides) and emits the `xGetDeviceKeyMappingReply` wire layout (`write_get_device_key_mapping_reply`, byte[1]=24, keySymsPerKeyCode@byte[8]). Mirrors Xorg `Xi/getkmap.c` (both core + XI1 derive from `XkbGetCoreMap`). Tests `get_device_key_mapping_reply_matches_xiproto_layout`, `get_device_key_mapping_reply_empty_is_header_only`, `xi_get_device_key_mapping_matches_core_keyboard_mapping`, `xi_get_device_key_mapping_on_pointer_device_is_bad_match`.
  - **FOLLOW-UP — RESOLVED 2026-06-18:** the *write* path `XChangeDeviceKeyMapping` (XI1 minor 25) now stores into `keymap_overrides`, so the `XChangeDeviceKeyMapping → XGetDeviceKeyMapping` round-trip reflects the change. Extracted a shared `store_keymap_overrides` helper used by both core `ChangeKeyboardMapping` (opcode 100) and XI1 minor 25 (it now reads `keySymsPerKeyCode`@body[2], previously ignored). Test `xi_change_device_key_mapping_round_trips_via_get` asserts the override store + the minor-24 read-back. Was (found via vng XTS, `XGetDeviceKeyMapping-3` PASS→FAIL): minor 25 validated + emitted MappingNotify but never wrote `keymap_overrides`. Remaining zero-stub block (umbrella TODO `:11810`): GetSelectedExtensionEvents (7) — **deferred**, needs a window-vs-device selection-model decision (yserver stores `DevicePropertyNotify`/`MappingNotify`/`ChangeDeviceNotify` selections device-scoped, ignoring the window arg, so a window-scoped reply would under-report; same flavour as the Xinerama deferral).
- [x] **XI1 GetFeedbackControl** (minor 22) — **REAL implementation.** Reports the device class's single default feedback (id 0): a `KbdFeedbackState` for key devices built from `state.keyboard_control`, a `PtrFeedbackState` for pointer devices from `state.pointer_control` — the *same* state core `GetKeyboardControl`/`GetPointerControl` expose, mirroring Xorg `Xi/getfctl.c` (where the KbdFeedback ctrl *is* the keyboard control). `num_feedbacks=1` is faithful to yserver's single-keyboard/single-pointer model. New encoders `encode_kbd_feedback_state`, `encode_ptr_feedback_state`, `write_get_feedback_control_reply`. Tests `get_feedback_control_kbd_reply_layout`, `get_feedback_control_ptr_state_layout`, `xi_get_feedback_control_kbd_mirrors_keyboard_control`, `xi_get_feedback_control_ptr_mirrors_pointer_control`.
- [x] **XI1 GetDeviceMotionEvents** (minor 10) — **REAL (faithful empty-history) reply.** yserver keeps no per-device motion buffer (same posture as core `GetMotionEvents`, accepted by-design), so it now returns the proper `xGetDeviceMotionEventsReply` with `RepType=10`, `nEvents=0`, `axes`=the device's valuator count, `mode=Absolute` — matching Xorg `Xi/gtmotion.c`'s empty-history path (not a zeroed catch-all reply). `write_get_device_motion_events_reply`. Tests `get_device_motion_events_reply_empty_history_layout`, `xi_get_device_motion_events_pointer_returns_empty_history`, `xi_get_device_motion_events_keyboard_is_bad_match`.
- [!] **XINERAMA GetScreenSize** — investigated 2026-06-17: **NOT a clean fix, do not naively patch.** Xorg `ProcPanoramiXGetScreenSize` makes `screen` index the *protocol* screens (`screenInfo.screens[]`, usually 1) and returns that screen's *union* width/height — so yserver returning the global size is actually correct for screen 0. "Fixing" it to index per-monitor would INTRODUCE a divergence. The only real gap is the missing `screen >= PanoramiXNumScreens → BadMatch` guard — but yserver's `GET_SCREEN_COUNT` returns the *monitor* count, not a protocol-screen count, so yserver's Xinerama legacy model already differs from Xorg's. Resolving needs a model decision (monitors-as-screens vs protocol-screens), not a quick patch. Left untouched.
- [ ] **MIT-SCREEN-SAVER SetAttributes** always BadAccess (`:6804`).
- [ ] **GetMotionEvents** (core 39) returns `nevents=0` (`:16078`) — by-design (setup advertises `motion_buffer_size:0`), but no history kept.
- [ ] **Bell** (core 104) validates then no-ops (`:16689`); no XKB BellNotify; stored bell/click control never consumed.

## Tier 5 — Mechanical debt

- [ ] **2,381** `.unwrap()/.expect()` in non-test code — large panic surface; do a focused pass on those reachable from client-controlled input.
- [ ] **92** `panic!` sites — audit reachability.
- [ ] **54** `#[allow(dead_code)]` / `allow(unused)` — review for genuinely dead vs. consumed-later.
- [ ] **22** `TODO` + **326** stub-ish comments ("for now"/"placeholder"/"not yet"/"stub") — triage.
- [ ] 16 `unimplemented!()` (RecordingBackend) — benign; leave or document as intentional.

## Verify — possible live regression

- [x] **XC-MISC namespace coverage** — VERIFIED CLEAN, false alarm. `xid_occupied` (`server.rs:1429`) covers 17 namespaces = 8 ResourceTables inside `resources.xid_in_use()` (`resources.rs:343`) + 9 explicit extension maps. The agent counted the 10 call-lines, missing that `xid_in_use` covers 8. Cross-checked every `HashMap<u32,_>` field in `ServerState`: the other u32 maps (`clients`, `idletime_last_evaluated`, `client_wm_class`, `close_down_modes`, `zombie_clients`) are keyed by client-id / system-counter, NOT allocatable XIDs → correctly excluded. Guarded by `xid_occupied_covers_every_namespace` test (`server.rs:4832`). No regression.

## Clean (no stubs found)

SHAPE (all 0–8), DAMAGE (all 0–4), DPMS (all 0–7 + SelectInput), BIG-REQUESTS, GenericEvent, core drawing/color/keymap paths, XC-MISC checker logic. Confirmed-real spot checks: AllocColor/QueryColors (real TrueColor decompose), CirculateWindow, KillClient/SetCloseDownMode, RANDR SetScreenSize/SetCrtcConfig (full validation + real modeset), XI QueryDeviceState (now derives real classes — old latent bug resolved), RENDER Trapezoids/gradients/Composite (real Vulkan v2 backend).

## Recommended fix order

1. Tier-1 catch-all hangs (trivial; hangs → errors).
2. XC-MISC namespace verify (potential live XID regression).
3. XFIXES barriers (you *advertise* v5 for Mutter).
4. GrabServer (core contract).
5. X-Resource queries (data exists, easy win for tooling).

## Re-prioritised under the Xorg-baseline lens (2026-06-18)

**New constraint (learned the hard way):** real Xorg on a TTY does NOT pass
XTS 100% — the XI suite alone fails ~47/316 cases on stock Xorg, the same
cases yserver "fails" (64-bit `KeySym` marshalling, deprecated XI 1.x).
See [[reference_xorg_not_100pct_on_xts_xi]]. So the target is *matching Xorg's
real-client behaviour*, NOT an XTS pass count. Before touching anything an
XTS case flags, diff against an Xorg baseline (`tools/xts-vs-baseline.py`) to
confirm it's winnable. Verify fixes with a real app / against Xorg, not xts.

**Where we CAN make a difference (real client breaks today, verifiable, backend can deliver, self-contained):**

1. **RANDR gamma** (Tier 4) — *standout.* `redshift` / `gammastep` / GNOME
   Night Light are flatly broken (`GetCrtcGammaSize`=0). Self-contained:
   report a real LUT size, read current gamma, apply `SetCrtcGamma` via KMS
   (`drmModeCrtcSetGamma` / atomic `GAMMA_LUT`). Verify: run redshift, screen
   warms. Highest impact-to-effort of the lot.
2. **GrabServer / UngrabServer** (Tier 3) — real WM menu / screensaver
   atomicity. yserver is single-core, so a server grab = stop dispatching
   *other* clients' requests until ungrab (+ auto-ungrab on grab-client
   disconnect). Tractable, core-correct. Verify: WM menu behaviour vs Xorg.
3. **RANDR output properties — EDID** (Tier 4) — `autorandr`, `arandr`, colour
   tools read EDID; the KMS backend already has the blob. Exposing the EDID
   property is concrete; backlight/scaling-mode can follow.
4. **RENDER CreateConicalGradient + AddTraps** (Tier 4) — register the picture
   XID (downstream Composite currently fails the lookup silently) and
   rasterise. Real cairo/GTK content. The v2 Vulkan backend already does
   trapezoids/gradients, so AddTraps/conical are incremental.

**Real but bigger / parked:** XKB `Set*` (runtime layout switch — large XKB
surface), MIT-SHM live shared pixmaps (OBS XSHM), SYNC blocking Await, PRESENT
flip path, DRI3 multi-plane, GLX TFP / RENDER convolution (no consumer reaches
them yet).

**Robustness track (separate):** a focused pass on the `.unwrap()/.expect()`
sites *reachable from client-controlled input* (Tier 5) — a buggy/hostile
client crashing the server is itself an Xorg divergence (Xorg is hardened).
Scope to the client-input-reachable subset, not all 2,381.

**Do NOT chase (XTS-only where Xorg also fails, or needs a model decision):**
XINERAMA `GetScreenSize`, XI1 `GetSelectedExtensionEvents`, XTEST
`CompareCursor`/`GrabControl`, the faked XI device topology (deliberately
non-zero to dodge the VSCode `ndevices=0` crash), `GetMotionEvents`
empty-history (Xorg-compatible by design), MIT-SCREEN-SAVER `SetAttributes`.
