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

- [ ] **XFIXES pointer barriers** CreatePointerBarrier/DeletePointerBarrier (minor 31/32) — not handled, **not even defined** in `xfixes.rs:58`; default arm `process_request.rs:4514` silently no-ops. XFIXES advertises **v5.0, forced for Mutter** (`xfixes.rs:54`). GNOME edge-resistance / pointer confinement silently break. **HIGH.**
- [ ] **X-Resource v1.2** — all 5 data queries return zeros: QueryClients `:8773`, QueryClientResources `:8780`, QueryClientPixmapBytes `:8787`, QueryClientIds `:8794`, QueryResourceBytes `:8801`. Backing data exists in `ServerState` → fillable. `xrestop`/resource monitors see nothing.
- [ ] **GLX `GLX_EXT_texture_from_pixmap`** — advertised, but `BindTexImageEXT` (`:9288`) binds and never updates content (`TODO(glx-tfp)`). (Matches parked TFP work / `2026-06-09-glx-tfp-radv-export-rootcause.md`.)
- [ ] **RENDER QueryFilters** (`:1818`) advertises `convolution`/`bilinear` that `SetPictureFilter` (`:1827`)/backend won't apply (only `nearest` honored). (Matches parked render-convolution work.)

## Tier 3 — Silent wrong behavior / broken contracts

- [ ] **GrabServer / UngrabServer** (core 36/37) — `process_request.rs:202–203`, pure no-ops, **zero backing state** (no `server_grab` anywhere). Test asserts it does nothing (`grab_server_is_log_only_no_op:27769`). WM/toolkit server-grab atomicity (menu grabs, screensaver) silently broken. Highest-impact *core* gap.
- [ ] **XKB read-only façade** (`kms/xkb.rs`) — every `Set*` (SetMap `:15896→None`, SetControls/SetNames/SetCompatMap/indicators) silently swallowed; `GetState` (`xkb.rs:786`) all-zero; `GetMap` (`xkb.rs:301`) drops AltGr/secondary groups; `GetNames` (`xkb.rs:651`) hardcoded "evdev+aliases(qwerty)" not RMLVO-derived. Client keyboard reconfig = success-shaped silence.
- [x] **QueryFont / QueryTextExtents** (core 47/48) — FIXED: invalid fontable now → `BadFont` (was `unwrap_or_default()` zeroed reply). Tests `query_font_invalid_fontable_returns_bad_font`, `query_text_extents_invalid_fontable_returns_bad_font`.
- [ ] **SYNC Await / AwaitFence** (`:2947`/`:3144`) — counters/alarms/fences work, but blocking-await never suspends the client stream (self-described "known gap" `:3179`).
- [ ] **XTEST CompareCursor** always `same=true` (`:5934`); **GrabControl** no-op (`:5964`).
- [ ] **RecolorCursor** (core 96) — `process_request.rs:204`, accepted-and-dropped no-op.
- [x] **AllocNamedColor / LookupColor** (core 85/92) — FIXED: unknown name now → `BadName` (was hardcoded gray `0xc0c0`). Tests `alloc_named_color_unknown_name_returns_bad_name`, `lookup_color_unknown_name_returns_bad_name`.

## Tier 4 — Faked / partial data (works enough today, fidelity invented)

- [ ] **PRESENT** — always Copy path (no Flip/scanout, `flip_path==false`); `CompleteNotify.ust` hardcoded 0; synthetic per-window MSC; scheduler never drained for flips (`present_scheduler.rs:208` empty-Vec stub; selector inputs hardcoded `:7618`). OK for Mesa WSI now, risky for frame-pacing clients.
- [ ] **XI device topology** — `ListInputDevices`/`XIQueryDevice` report a hardcoded 4-device model w/ fabricated button/axis/range data (`process_request.rs:9570–9573`); only names real; XIQueryDevice fakes pointer pos to screen-center (`:9971`). (Deliberately non-zero per VSCode `ndevices=0` fatal-CHECK history — but class data invented.)
- [ ] **RANDR gamma absent** — `GetCrtcGammaSize`/`GetCrtcGamma` hardcoded size=0/empty (`randr.rs:755/768`); no `SetCrtcGamma` dispatch → redshift/gammastep/night-light broken (backend likely can do KMS gamma).
- [ ] **RANDR output properties absent** — ListOutputProperties/QueryOutputProperty/GetOutputProperty (`:2338/:2346/:2467`) → zero props / BadName; Change/Configure/Delete fall to catch-all. No EDID/Backlight/scaling-mode.
- [ ] **RANDR SetScreenConfig** (`:2601`) — explicit no-op accept (returns success, applies nothing). RandR-1.0 clients believe resolution changed.
- [ ] **GLX configs** synthesised to mirror radeonsi (`:8911/:8930`) — may mismatch other drivers; CREATE_CONTEXT/MAKE_CURRENT/SWAP_BUFFERS bookkeeping-only; QUERY_CONTEXT zero attribs.
- [ ] **MIT-SHM** `shared_pixmaps=false` (`:5013`); `CreatePixmap` (`:5238`) one-time snapshot, not live-shared.
- [ ] **DRI3** multi-plane `PixmapFromBuffers` hard-rejected if `num_buffers!=1` (`:8024`); `BuffersFromPixmap` hardcoded `nfd=1` (`:8200`).
- [ ] **COMPOSITE** redirect record-only on v1 backend (gated on `supports_redirect_activation()`, `:4558/:4576`); CreateRegionFromBorderClip fakes full-drawable rect (`:4727`).
- [ ] **RENDER CreateConicalGradient** (minor 36) — empty `// stub` (`:2063`); picture XID never registered → downstream Composite silently fails lookup. **AddTraps** (minor 32) — damages but never rasterizes (`:2066`).
- [ ] **XI1 GetDeviceKeyMapping** (minor 24) — zero reply, empty keymap on a key-class device (`:13062`). Plus zero-stub block (umbrella TODO `:11810`): GetSelectedExtensionEvents (7), GetFeedbackControl (22), GetDeviceMotionEvents (10).
- [ ] **XINERAMA GetScreenSize** (`:3347`) ignores `screen` index, returns global union (QueryScreens — the path real clients use — is correct).
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
