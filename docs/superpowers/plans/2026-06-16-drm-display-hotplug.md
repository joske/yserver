# DRM Display Hotplug Implementation Plan (rev 4)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Detect runtime DRM display hotplug (connect/disconnect, power on/off) on the KMS (`yserver-hw`) backend, bring up/tear down scanout for changed connectors, and emit RANDR `ScreenChangeNotify`/`OutputChangeNotify`/`CrtcChangeNotify` so clients react — closing GitHub issue #9.

**Architecture:** A udev monitor on the `drm` subsystem (approach **A** — in the KMS backend) exposes an fd the core poller watches under a new `BackendFdKind::DrmHotplug`/token. On readiness the core calls `Backend::on_display_hotplug(state)`. To survive bursty udev sequences, that hook does NOT re-probe inline; it arms a 150 ms debounce deadline (same machinery as `libinput_hotplug_retry_until`) serviced from `poll_deferred_input`. When the deadline fires, the backend: (1) GPU-quiesces via `scene.drain_all`, (2) re-probes connectors — extending `requery_outputs_and_modeset` to **add** newly-connected outputs and recompact the horizontal layout — (3) **rebuilds the scene's per-output state vector in lockstep** (the critical gap rev 1 missed), (4) rebuilds `state.randr` using a **stable per-connector RANDR ID allocator** (not positional renumbering), (5) fans out **per-output** RANDR notify events, (6) wakes the scene for a full repaint.

**Tech Stack:** Rust, `udev` crate (0.9, already transitively resolved), `drm` crate, mio poller, the v2 KMS backend (`KmsBackendV2`/`PlatformBackend`/`SceneCompositor`).

---

## Why rev 2 (codex review of rev 1, gpt-5.4-mini)

Rev 1 was rejected on five findings; rev 2 addresses each:

1. **Critical — scene-side output vector was never updated.** `SceneCompositor` holds its own `inner.outputs: Vec<OutputSceneState>` (per-output GPU state: descriptor ring, damage history, cursor state, cached extent) built once at `SceneCompositor::new` and indexed in lockstep with `platform.outputs` via `.expect("range")`. Mutating `platform.outputs` alone → new output never composited, dropped output → index desync / panic. **Fix: Task 6** adds a scene rebuild, run after a full GPU drain.
2. **High — positional RANDR IDs renumber survivors.** `randr_outputs()` assigned `output_id = i+1` / `crtc_id = n+i+1`; unplugging one monitor renumbered the others, breaking clients caching IDs (Xorg allocates each connector's ID once and never renumbers — `rroutput.c:83`, `rrcrtc.c:75`). **Fix: Task 5** — a connector-keyed stable ID allocator.
3. **High — emit reported `outputs.first()`, not the changed output.** Wrong object announced for multi-head. **Fix: Task 3** makes the helper per-output.
4. **Medium-high — debounce was promised but unimplemented; heavy DRM/Vulkan work ran inline on the poll thread; `kms_outputs_active = true` set unconditionally.** **Fix: Tasks 2 + 8** — real debounce timer + DPMS-aware gate.
5. **Medium — tautological tests.** **Fix:** fb-extent test now grounded in issue #9's own captured value (`5120x1440`); stable-ID test asserts the real invariant (survivor keeps its ID across a drop).

## Why rev 3 (codex re-review of rev 2, gpt-5.4-mini)

Rev 2 cleared findings #2 (no positional-ID assumptions found anywhere in the tree), #4 (debounce wiring sound — `poll_deferred_input` runs every loop, `next_wakeup` drives the poll timeout), and #5 (tests well-grounded). Three blockers remained:

6. **High — `scene.drain_all` is NOT a full GPU quiesce.** It waits `pending_acks` tickets + `pending_pool_releases`, but NOT `failed_submit_bos` (the "GPU submitted, atomic commit failed" frames, which keep a fence ticket + BO slot until they retire later). Dropping/rebuilding `OutputSceneState` while one is in flight races live GPU reads. The suspend path calls `platform.wait_idle_bounded()` (`backend.rs:4621`, a 5 s-bounded `device_wait_idle`) *before* `drain_all` for exactly this reason. **Fix (Task 7):** the rebuild quiesce is `wait_idle_bounded()` THEN `drain_all()` — full device idle first, then descriptor-slot release.
7. **High — dropped outputs become unqueryable after the notify.** Rev 2 rebuilt `state.randr` from live outputs only, then emitted a `(id, 0, 0)` tuple for a gone output — but `output_info`/`crtc_info` (`randr.rs:213,249`) search only `randr.outputs`, so a client that reacts to `OutputChangeNotify` by querying that id gets `BadOutput`. Xorg keeps the output resource and marks it disconnected (`rroutput.c:339`). **Fix (new Task 5b):** persist every connector yserver has seen in `randr.outputs` with a `connected` flag; disconnected ones report `connection = Disconnected`, `crtc = 0`. The wire already carries `connection` (currently hardcoded `0` at `process_request.rs:2212`).
8. **Medium-high — the rebuild is not transactional.** Rev 2 rebuilt `state.randr` then logged-and-continued on scene-rebuild failure, leaving scene/platform vectors desynced → next-tick `expect("range")` panic. **Fix (Task 7):** quiesce → rebuild scene FIRST; treat scene-rebuild failure as fatal (`request_exit()`, same as resume's card-gone path) so there is no desynced-and-running state.

## Why rev 4 (codex re-review of rev 3, gpt-5.4-mini)

Rev 3 cleared #6/#8 (quiesce sequence + transactional rebuild confirmed correct, no `GetCrtcInfo` break). But the **new** Task 5b (persistent connectors) introduced three of its own, all now fixed in rev 4:

9. **Bogus zero-mode advertised.** Disconnected stubs carry `mode_id = 0`, and `from_outputs`' mode-dedup looped over *all* outputs → a phantom 0-mode landed in `state.modes` and `GetScreenResourcesCurrent`. **Fix:** Task 5b Step 1b filters the mode loop on `out.connected`.
10. **Primary could be a disconnected output.** `primary_output = outputs.first()` (sorted by id) picks a disconnected connector when a lower-id monitor is unplugged. **Fix:** Step 1b sets primary to the first *connected* output.
11. **Incomplete compile surface.** The new `connected` field was missing from literals in `nested.rs:378` and `process_request.rs:23565/23578/32931` → wouldn't build. **Fix:** Step 1 now lists every literal site (verified via `rg`).

---

## Background: verified call-graph (read before starting)

- **One-time enumeration:** `crates/yserver/src/drm/modeset.rs:193 discover_outputs()`, called from `kms/backend.rs:465` (init) and `kms/v2/platform.rs:2325` (resume re-probe).
- **Existing re-probe (survivors/drops only, NO add, NO x-recompact):** `kms/v2/platform.rs:2323 requery_outputs_and_modeset()`.
- **Initial layout + pool allocation (template for the add path):** horizontal `next_x` loop `kms/backend.rs:470-505`; `ScanoutBoPool::allocate(vk, device, w, h, 3, &output.scanout_modifiers)` per output `kms/v2/platform.rs:734-765`.
- **RANDR source-of-truth:** `crates/yserver-core/src/randr.rs` — `RandrState::from_outputs(timestamp, Vec<RandrOutput>)` trusts caller-assigned IDs and sets `primary_output = outputs.first()`. KMS produces the vec via `kms/v2/backend.rs:2359 randr_outputs()` (the positional allocator we replace in Task 5).
- **RANDR emit (to refactor, per-output):** `core_loop/run.rs:854 handle_host_container_resize()`; fanout `:918-991` (`RANDR_FIRST_EVENT = 89`; reads `outputs.first()`; snapshots `state.randr_select_masks`; writes via `client_io::write_or_buffer`).
- **Poll token plumbing:** `core_loop/poll_tokens.rs:32-47` (next free `Token(8)`); registration `run.rs:359-369`; dispatch `run.rs:489-540`.
- **Per-iteration backend hooks:** `run.rs:418 next_wakeup()`, `:444 before_block()`, `:769 poll_deferred_input(state)`, `:776 maybe_composite()`.
- **Trait seam:** `backend/trait_def.rs:32 BackendFdKind`; `:311 on_page_flip_ready`; `:509 poll_fds`.
- **Scene:** `kms/v2/scene.rs` — `SceneCompositor` (`:424`, field `inner: Option<SceneCompositorInner>`, no lock); `SceneCompositorInner` (`:469`, fields incl. `outputs: Vec<OutputSceneState>`, `deferred_upload_wait_set: HashSet<usize>`); `OutputSceneState` (`:314-386`); construction loop `:566-599`; `tick()` loops `0..inner.outputs.len()` and `tick_one_output` reads `platform.outputs[output_idx]` (`:1561`); `expect("range")` indexers `:1381,1406,1473,1482,1534,1558,1582,1643`. **No existing scene resize path.** Owned by `KmsBackendV2.scene` (`backend.rs:167`).
- **GPU quiesce (reuse for safe rebuild):** `self.scene.drain_all(&mut self.platform)` — used by `run_suspend` at `backend.rs:4264` (`device_wait_idle` + pool destruction).
- **Debounce template:** `KmsBackendV2.libinput_hotplug_retry_until: Option<Instant>` (field `:386`); armed in `on_libinput_ready` (`:9517`); serviced in `poll_deferred_input` (`:9529`); chained in `next_wakeup` (`:8957`).
- **Resume wiring (template):** `run_resume()` (`:4699`) → `requery_outputs_and_modeset()` → `fire_randr_changes()` (`:4807`, log-only); `kms_outputs_active = true` at `:4741`.

### Design decisions (locked)

1. **Monitor placement: A (KMS backend).**
2. **New-output placement: extend-right, with full x-recompact on every topology change.** After any add/drop, re-lay all surviving outputs left-to-right from x=0 in connector order, so a drop never leaves a gap and RANDR geometry matches scanout geometry. A WM/DM with its own RANDR config repositions afterward; bare-server default is extend (matches Xorg modesetting).
3. **Stable RANDR IDs keyed by connector name (Task 5).** Each DRM connector gets an output_id + crtc_id allocated once from a monotonic counter and reused for the connector's whole session lifetime (including disconnect→reconnect). Modes deduped by `(w,h,vrefresh)` into the same monotonic space. IDs are never renumbered and never reused for a different connector. Mirrors Xorg's per-connector `RROutput`/`RRCrtc` model.
4. **Stable primary:** `randr_outputs()` returns outputs sorted by ascending `output_id`, so `from_outputs`' `outputs.first()` is the earliest-allocated connector — stable across hotplug.
5. **Debounced, off-the-poll-thread-ish re-probe (Tasks 2 + 8).** `on_display_hotplug` only drains the monitor + arms a 150 ms deadline. The expensive re-probe/rebuild runs once from `poll_deferred_input` when the deadline fires, coalescing a udev burst into one pass.
6. **Safe scene rebuild via drain (Task 6).** A topology change is rare; treat it like a mini suspend/resume: `scene.drain_all` (wait all GPU fences) → rebuild `inner.outputs` fresh from `platform.outputs` → full repaint. This sidesteps in-flight fence-free hazards and index-renumbering bugs entirely.
7. **DPMS-aware:** the re-probe sets `kms_outputs_active = true` only when DPMS power level is On; it must not silently undo an intentional DPMS-off.
8. **Linux-only.** `udev` is Linux-specific; the monitor is `#[cfg(target_os = "linux")]`. Non-Linux KMS hotplug is a documented deferral (not a stub).

### Testability note

GPU/DRM work (`ScanoutBoPool::allocate`, `commit_modeset`, scene rebuild) is HW-only (Task 9). Unit-testable pieces are isolated: udev wrapper construction/drain (T1); token/dispatch (T2); per-output emit fanout with a fake subscriber (T3); fb-extent recompute grounded in issue #9's captured `5120x1440` (T4); the stable-ID allocator's "survivor keeps ID across drop" invariant (T5); debounce arm/expire on `next_wakeup`/`poll_deferred_input` (T8). **Never assert a fabricated value** — fb-extent expectations come from the issue log; ID-stability is a structural invariant, not arithmetic.

---

## Task 1: udev DRM hotplug monitor wrapper

**Files:**
- Modify: `Cargo.toml` (`[workspace.dependencies]`), `crates/yserver/Cargo.toml` (`[dependencies]`)
- Create: `crates/yserver/src/kms/hotplug.rs`; Modify: `crates/yserver/src/kms/mod.rs`

- [ ] **Step 1: Add the udev dependency.** In root `Cargo.toml` `[workspace.dependencies]` (where `input = "0.10"` lives) add `udev = "0.9"`. In `crates/yserver/Cargo.toml` `[dependencies]` add `udev.workspace = true`.

- [ ] **Step 2: Write the failing test.** Create `crates/yserver/src/kms/hotplug.rs`:

```rust
//! DRM display-hotplug detection (issue #9). A udev monitor on the
//! `drm` subsystem; its fd is registered with the core poller under
//! `BackendFdKind::DrmHotplug`. On readiness the backend arms a
//! debounce window and re-probes connectors. Linux-only — non-Linux
//! targets get a `None` monitor (documented platform gap, not a stub).

#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, RawFd};

#[cfg(target_os = "linux")]
pub(crate) struct DrmHotplugMonitor {
    socket: udev::MonitorSocket,
}

#[cfg(target_os = "linux")]
impl DrmHotplugMonitor {
    /// Open a udev monitor for `drm` uevents (hotplug/HPD). Returns
    /// `Ok(None)` if udev is unavailable so the backend continues
    /// without hotplug rather than failing to start.
    pub(crate) fn new() -> std::io::Result<Option<Self>> {
        let builder = match udev::MonitorBuilder::new() {
            Ok(b) => b,
            Err(e) => {
                log::warn!("drm hotplug: udev monitor unavailable: {e}; hotplug disabled");
                return Ok(None);
            }
        };
        let socket = builder.match_subsystem("drm")?.listen()?;
        log::info!("drm hotplug: udev monitor listening on `drm` subsystem");
        Ok(Some(Self { socket }))
    }

    pub(crate) fn raw_fd(&self) -> RawFd {
        self.socket.as_raw_fd()
    }

    /// Drain all pending uevents (non-blocking, edge-triggered). Returns
    /// `true` if a connector change worth re-probing was seen.
    pub(crate) fn drain(&mut self) -> bool {
        let mut saw_change = false;
        for event in self.socket.iter() {
            let action = event.action().and_then(|a| a.to_str()).unwrap_or("");
            log::debug!("drm hotplug: uevent action={action:?}");
            if matches!(action, "change" | "add" | "remove") {
                saw_change = true;
            }
        }
        saw_change
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    #[test]
    fn monitor_opens_and_drains_without_blocking() {
        match super::DrmHotplugMonitor::new() {
            Ok(Some(mut m)) => {
                assert!(m.raw_fd() >= 0);
                let _ = m.drain(); // no physical hotplug → returns immediately
            }
            Ok(None) => { /* udev unavailable in this env — acceptable */ }
            Err(e) => panic!("monitor construction errored unexpectedly: {e}"),
        }
    }
}
```

Add `pub(crate) mod hotplug;` to `crates/yserver/src/kms/mod.rs`.

- [ ] **Step 3: Run to verify it compiles/passes.** `cargo test -p yserver --locked kms::hotplug 2>&1 | tail -20` → PASS.
- [ ] **Step 4: Format + lints.** `cargo +nightly fmt && cargo clippy -p yserver --locked 2>&1 | grep -E "warning|error" | head` → no new warnings.
- [ ] **Step 5: Commit.**

```bash
git add Cargo.toml Cargo.lock crates/yserver/Cargo.toml crates/yserver/src/kms/hotplug.rs crates/yserver/src/kms/mod.rs
git commit -m "feat(kms): add udev drm-subsystem hotplug monitor wrapper (issue #9)"
```

---

## Task 2: BackendFdKind::DrmHotplug + token + on_display_hotplug hook

**Files:** `crates/yserver-core/src/backend/trait_def.rs`, `crates/yserver-core/src/core_loop/poll_tokens.rs`, `crates/yserver-core/src/core_loop/run.rs`

- [ ] **Step 1: Write the failing token test.** Add to `poll_tokens.rs`:

```rust
#[cfg(test)]
mod hotplug_token_tests {
    use super::*;
    #[test]
    fn drm_hotplug_token_is_distinct() {
        let toks = [
            LISTENER_TOKEN.0, DRM_TOKEN.0, SIGNAL_TOKEN.0, LIBINPUT_TOKEN.0,
            HOST_X11_TOKEN.0, PRESENT_COMPLETION_TOKEN.0, SEAT_TOKEN.0, DRM_HOTPLUG_TOKEN.0,
        ];
        let unique: std::collections::HashSet<_> = toks.iter().collect();
        assert_eq!(unique.len(), toks.len(), "all poll tokens must be distinct");
    }
}
```

- [ ] **Step 2: Run → FAIL** (`cannot find value DRM_HOTPLUG_TOKEN`). `cargo test -p yserver-core --locked poll_tokens 2>&1 | tail`.

- [ ] **Step 3: Add the token.** After `pub const SEAT_TOKEN: Token = Token(7);`:

```rust
/// udev DRM-subsystem hotplug monitor fd; readiness drives
/// `Backend::on_display_hotplug` (issue #9). KMS-only.
pub const DRM_HOTPLUG_TOKEN: Token = Token(8);
```

Add `DRM_HOTPLUG_TOKEN` to the reserved-token arrays in this file (around `:120-165`) next to `SEAT_TOKEN`.

- [ ] **Step 4: Add the `BackendFdKind` variant** (after `Seat,` at `trait_def.rs:50`):

```rust
    /// udev monitor fd on the `drm` subsystem (KMS, Linux). Readiness
    /// drives `Backend::on_display_hotplug` (issue #9).
    DrmHotplug,
```

- [ ] **Step 5: Add the trait hook** (after `on_page_flip_ready`, `:311`):

```rust
    /// The DRM-subsystem udev hotplug fd is readable. The backend
    /// drains the monitor and arms a debounce window; the actual
    /// connector re-probe + RANDR fanout runs later from
    /// `poll_deferred_input`. Default: no-op. Issue #9.
    fn on_display_hotplug(&mut self, _state: &mut ServerState) {}
```

- [ ] **Step 6: Wire run.rs.** Add `DRM_HOTPLUG_TOKEN` to imports (`:27-28`). In the registration `match kind` add `BackendFdKind::DrmHotplug => DRM_HOTPLUG_TOKEN,`. In the dispatch loop after the `DRM_TOKEN` arm add:

```rust
                DRM_HOTPLUG_TOKEN => {
                    backend.on_display_hotplug(state);
                }
```

- [ ] **Step 7: Run → PASS.** `cargo test -p yserver-core --locked poll_tokens 2>&1 | tail`.
- [ ] **Step 8: Build + fmt.** `cargo build --locked 2>&1 | tail && cargo +nightly fmt`.
- [ ] **Step 9: Commit.**

```bash
git add crates/yserver-core/src/backend/trait_def.rs crates/yserver-core/src/core_loop/poll_tokens.rs crates/yserver-core/src/core_loop/run.rs
git commit -m "feat(core): add DrmHotplug fd kind, poll token, on_display_hotplug hook (issue #9)"
```

---

## Task 3: Per-output RANDR change-notify helper (fixes finding #3)

**Files:** `crates/yserver-core/src/core_loop/run.rs`

The helper must announce the actual changed output/CRTC, not `outputs.first()`. It emits one screen-wide `ScreenChangeNotify` plus, for each output id in a caller-supplied list, the matching `CrtcChangeNotify`/`OutputChangeNotify`. A dropped output is announced with `crtc = 0` (None) and `mode = 0`.

- [ ] **Step 1: Write the failing test.** In the run.rs `#[cfg(test)] mod tests` (reuse the nearest existing helper that builds a `ServerState` + fake client; do NOT invent helper names):

```rust
    #[test]
    fn emit_randr_change_notifications_is_per_output() {
        let mut state = test_state_with_two_outputs(); // reuse/extend existing helper
        let client_id = 1u32;
        register_fake_client(&mut state, client_id);
        let win = crate::resources::ROOT_WINDOW;
        let mask = yserver_protocol::x11::randr::NOTIFY_MASK_SCREEN_CHANGE
            | yserver_protocol::x11::randr::NOTIFY_MASK_CRTC_CHANGE
            | yserver_protocol::x11::randr::NOTIFY_MASK_OUTPUT_CHANGE;
        state.randr_select_masks.insert((client_id, win), mask);

        // Announce BOTH outputs as changed.
        let changed: Vec<(u32, u32, u32)> = state
            .randr
            .outputs
            .iter()
            .map(|o| (o.output_id, o.crtc_id, o.mode_id))
            .collect();
        let before = fake_client_outbuf_len(&state, client_id);
        super::emit_randr_change_notifications(&mut state, &changed);
        let after = fake_client_outbuf_len(&state, client_id);

        // 1 ScreenChange (32) + per output: Crtc (32) + Output (32) = 32 + 2*64 = 160.
        assert_eq!(after - before, 160, "1 screen + 2 outputs × (crtc+output) events");
    }
```

> If run.rs has no two-output state helper, extend the existing single-output one to push a second `RandrOutput`; reuse `register_fake_client` / `fake_client_outbuf_len` verbatim from the nearest existing test. The 32-byte event size is from the wire format (RANDR notify events are fixed 32 bytes — `randr.rs` encoders), not invented.

- [ ] **Step 2: Run → FAIL** (`emit_randr_change_notifications` not found / arity mismatch).

- [ ] **Step 3: Add the helper + a `ChangedOutput` shape.** Define the per-output triple inline as `(output_id, crtc_id, mode_id)` and write:

```rust
/// Fan out RANDR notify events for a topology/geometry change. Emits one
/// screen-wide `ScreenChangeNotify` per subscriber, then a
/// `CrtcChangeNotify` + `OutputChangeNotify` for each `(output_id,
/// crtc_id, mode_id)` in `changed`. A dropped output is passed as
/// `(output_id, 0, 0)`. Geometry is read from `state.randr`.
pub fn emit_randr_change_notifications(
    state: &mut ServerState,
    changed: &[(u32, u32, u32)],
) {
    use std::sync::atomic::Ordering;
    use yserver_protocol::x11::{SequenceNumber, randr as x11randr};
    const RANDR_FIRST_EVENT: u8 = 89;

    let timestamp = state.randr.timestamp;
    let width = state.randr.screen_width;
    let height = state.randr.screen_height;
    let width_mm = u16::try_from(state.randr.width_mm).unwrap_or(u16::MAX);
    let height_mm = u16::try_from(state.randr.height_mm).unwrap_or(u16::MAX);

    // Per-output position for CrtcChangeNotify: look up x/y from randr.
    let pos_of = |crtc: u32| -> (i16, i16) {
        state
            .randr
            .outputs
            .iter()
            .find(|o| o.crtc_id == crtc)
            .map_or((0, 0), |o| (o.x, o.y))
    };

    let subscribers: Vec<(u32, yserver_protocol::x11::ResourceId, u16)> = state
        .randr_select_masks
        .iter()
        .map(|((owner, window), mask)| (*owner, *window, *mask))
        .collect();

    for (owner, request_window, mask) in subscribers {
        let Some(client) = state.clients.get_mut(&owner) else { continue };
        let sequence = SequenceNumber(client.last_sequence.load(Ordering::Relaxed));
        if mask & x11randr::NOTIFY_MASK_SCREEN_CHANGE != 0 {
            let event = x11randr::encode_screen_change_notify_event(
                client.byte_order, RANDR_FIRST_EVENT, sequence,
                x11randr::ScreenChangeNotify {
                    timestamp, config_timestamp: timestamp,
                    root: crate::resources::ROOT_WINDOW.0,
                    request_window: request_window.0,
                    width, height, width_mm, height_mm,
                },
            );
            let _ = client_io::write_or_buffer(client, &event);
        }
        for &(output, crtc, mode) in changed {
            let (cx, cy) = pos_of(crtc);
            if mask & x11randr::NOTIFY_MASK_CRTC_CHANGE != 0 {
                let event = x11randr::encode_crtc_change_notify_event(
                    client.byte_order, RANDR_FIRST_EVENT, sequence,
                    x11randr::CrtcChangeNotify {
                        timestamp, request_window: request_window.0,
                        crtc, mode, x: cx, y: cy, width, height,
                    },
                );
                let _ = client_io::write_or_buffer(client, &event);
            }
            if mask & x11randr::NOTIFY_MASK_OUTPUT_CHANGE != 0 {
                let event = x11randr::encode_output_change_notify_event(
                    client.byte_order, RANDR_FIRST_EVENT, sequence,
                    x11randr::OutputChangeNotify {
                        timestamp, config_timestamp: timestamp,
                        request_window: request_window.0, output, crtc, mode,
                    },
                );
                let _ = client_io::write_or_buffer(client, &event);
            }
        }
    }
}
```

- [ ] **Step 4: Update `handle_host_container_resize`** to call the helper with its single output (the resize path changes only output 0's geometry):

```rust
    let changed: Vec<(u32, u32, u32)> = state
        .randr
        .outputs
        .first()
        .map(|o| (o.output_id, o.crtc_id, o.mode_id))
        .into_iter()
        .collect();
    emit_randr_change_notifications(state, &changed);
```

Delete the old inline fanout block (`:918-994`). Note the CrtcChange `x/y` for resize now come from `randr.outputs` (which `resize()` set to 0,0) rather than the host event's `ev.x/ev.y`; for the ynest single-output container this is 0,0 anyway — confirm the existing resize test still passes, and if it asserts `ev.x/ev.y`, keep resize passing its event coords by special-casing (acceptable: resize is single-output).

- [ ] **Step 5: Run new + existing resize tests → PASS.** `cargo test -p yserver-core --locked randr 2>&1 | tail -30`.
- [ ] **Step 6: fmt + clippy + commit.**

```bash
cargo +nightly fmt && cargo clippy -p yserver-core --locked 2>&1 | grep -E "warning|error" | head
git add crates/yserver-core/src/core_loop/run.rs
git commit -m "refactor(core): per-output emit_randr_change_notifications (issue #9, finding #3)"
```

---

## Task 4: Re-probe ADD path + x-recompact + RescanResult (fixes part of #1, #4)

**Files:** `crates/yserver/src/kms/v2/platform.rs`

- [ ] **Step 1: Define the result type** (near `requery_outputs_and_modeset`):

```rust
/// Outcome of a connector re-probe (issue #9). `dropped_old_indices`
/// are indices into the PRE-rescan `outputs` vector (descending), so the
/// scene side can replay the exact same removals; `added_count` new
/// outputs are appended at the end. Names are for logging + RANDR.
#[derive(Debug, Default)]
pub(crate) struct RescanResult {
    pub added_names: Vec<String>,
    pub dropped_names: Vec<String>,
    pub dropped_old_indices: Vec<usize>, // descending
    pub added_count: usize,
}
```

- [ ] **Step 2: Write the failing fb-extent test, grounded in issue #9's captured log.** Issue #9 reports `connector=DP-1 ... connector=HDMI-A-1 ... 2 outputs, fb 5120x1440` (two 2560×1440 panels). Add to the platform test module:

```rust
    #[test]
    fn recompute_fb_extent_matches_issue9_dual_2560x1440() {
        // External ground truth: issue #9 log shows DP-1 + HDMI-A-1,
        // both 2560x1440, extend-right → "fb 5120x1440".
        let layouts = &[(0i32, 2560u16, 1440u16), (2560i32, 2560u16, 1440u16)];
        assert_eq!(super::PlatformBackend::recompute_fb_extent_from(layouts), (5120, 1440));
    }
```

- [ ] **Step 3: Run → FAIL** (`recompute_fb_extent_from` not found).

- [ ] **Step 4: Add the pure helper + extend the re-probe.** Add (mirrors `kms/backend.rs:517-526`):

```rust
    /// Pure recompute of the virtual-screen extent from `(x, width,
    /// height)` triples. `fb_w = max(x + width)`, `fb_h = max(height)`.
    pub(crate) fn recompute_fb_extent_from(layouts: &[(i32, u16, u16)]) -> (u16, u16) {
        let fb_w = layouts.iter()
            .map(|(x, w, _)| u16::try_from(x.saturating_add(i32::from(*w))).unwrap_or(u16::MAX))
            .max().unwrap_or(0);
        let fb_h = layouts.iter().map(|(_, _, h)| *h).max().unwrap_or(0);
        (fb_w, fb_h)
    }

    /// Re-lay all current outputs left-to-right from x=0 in vector order
    /// (extend-right). Called after add/drop so a removed output never
    /// leaves a gap and RANDR geometry matches scanout geometry.
    fn recompact_horizontal_layout(&mut self) {
        let mut next_x: i32 = 0;
        for layout in &mut self.outputs {
            layout.x = next_x;
            layout.y = 0;
            next_x = next_x.saturating_add(i32::from(layout.width));
        }
    }
```

Change `requery_outputs_and_modeset` to return `RescanResult`:
- Collect `dropped_old_indices` (the descending index list it already computes at `:2403-2410`) and `dropped_names`.
- **NEW add loop:** for each discovered connector whose `connector_name` is not in `self.outputs`, replicate the init bring-up: allocate two/three buffers + `ScanoutBoPool::allocate(vk, device, w, h, 3, &output.scanout_modifiers)`, `commit_modeset` with the first registered fb, push `OutputLayout`, `Some(pool)` into `scanout_pools`, `vec![BoGenerationEntry::default(); n]` into `bo_generations`, `false` into `first_pageflip_logged`; record name in `added_names`, bump `added_count`. **Guard on `self.vk.as_ref()`** so the `for_tests` fixture (no Vk) takes the no-op branch.
- After add+drop, call `self.recompact_horizontal_layout()`, then recompute `self.fb_w`/`self.fb_h` via `recompute_fb_extent_from` over `self.outputs.iter().map(|l| (l.x, l.width, l.height))`.
- Return `RescanResult { added_names, dropped_names, dropped_old_indices, added_count }`.

- [ ] **Step 5: Fix the resume caller.** In `backend.rs:4713`, `requery_outputs_and_modeset()` now returns `RescanResult`. Use `.dropped_names` where the old code logged `dropped`, and pass the whole `RescanResult` to `fire_randr_changes` (rewritten in Task 7). Resume will now also drive the scene rebuild (Task 6) + per-output emit — a strict improvement over the old log-only behaviour.

- [ ] **Step 6: Run → PASS.** `cargo test -p yserver --locked recompute_fb_extent 2>&1 | tail`.
- [ ] **Step 7: Build + fmt + clippy + commit.**

```bash
cargo build --locked 2>&1 | tail && cargo +nightly fmt && cargo clippy -p yserver --locked 2>&1 | grep -E "warning|error" | head
git add crates/yserver/src/kms/v2/platform.rs crates/yserver/src/kms/v2/backend.rs
git commit -m "feat(kms): re-probe adds outputs + recompacts layout, returns RescanResult (issue #9)"
```

---

## Task 5: Stable per-connector RANDR ID allocator (fixes finding #2)

**Files:** `crates/yserver/src/kms/v2/backend.rs` (replace `randr_outputs`, add allocator field)

- [ ] **Step 1: Write the failing invariant test.** The real invariant: a surviving connector keeps its `output_id`/`crtc_id` across a drop of a *different* connector, and a brand-new connector gets a fresh (never-reused) id. Add to the backend test module:

```rust
    #[test]
    fn randr_ids_are_stable_across_drop() {
        let mut alloc = super::RandrIdAllocator::default();
        let a = alloc.ids_for("DP-1");
        let b = alloc.ids_for("HDMI-A-1");
        assert_ne!(a.output_id, b.output_id);
        // DP-1 unplugged, then re-queried later: same ids.
        let a2 = alloc.ids_for("DP-1");
        assert_eq!(a, a2, "a surviving/returning connector keeps its IDs");
        // A genuinely new connector gets ids never used before.
        let c = alloc.ids_for("DP-2");
        assert_ne!(c.output_id, a.output_id);
        assert_ne!(c.output_id, b.output_id);
        assert_ne!(c.crtc_id, a.crtc_id);
    }

    #[test]
    fn randr_mode_ids_dedup_by_resolution() {
        let mut alloc = super::RandrIdAllocator::default();
        let m1 = alloc.mode_id(2560, 1440, 60);
        let m2 = alloc.mode_id(2560, 1440, 60);
        let m3 = alloc.mode_id(1920, 1080, 60);
        assert_eq!(m1, m2, "same (w,h,vrefresh) shares a mode id");
        assert_ne!(m1, m3);
    }
```

- [ ] **Step 2: Run → FAIL** (`RandrIdAllocator` not found).

- [ ] **Step 3: Add the allocator + field.** Add to backend.rs:

```rust
/// Monotonic, connector-keyed RANDR ID allocator (issue #9). Each DRM
/// connector's output/CRTC ids are assigned once and reused for the
/// connector's whole session lifetime (incl. disconnect→reconnect);
/// ids are never renumbered or reused for a different connector. Modes
/// dedup by (w,h,vrefresh) into the same monotonic id space. Mirrors
/// Xorg's per-connector RROutput/RRCrtc model (rroutput.c:83,
/// rrcrtc.c:75 — allocate once, never renumber survivors).
#[derive(Debug, Default)]
pub(crate) struct RandrIdAllocator {
    next: u32,
    outputs: std::collections::HashMap<String, ConnectorIds>,
    modes: std::collections::HashMap<(u16, u16, u32), u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConnectorIds {
    pub output_id: u32,
    pub crtc_id: u32,
}

impl RandrIdAllocator {
    fn fresh(&mut self) -> u32 {
        self.next += 1;
        self.next
    }
    /// Stable (output_id, crtc_id) for a connector name.
    pub(crate) fn ids_for(&mut self, connector_name: &str) -> ConnectorIds {
        if let Some(ids) = self.outputs.get(connector_name) {
            return *ids;
        }
        let ids = ConnectorIds { output_id: self.fresh(), crtc_id: self.fresh() };
        self.outputs.insert(connector_name.to_string(), ids);
        ids
    }
    /// Stable mode id for a (w,h,vrefresh) tuple (deduped).
    pub(crate) fn mode_id(&mut self, w: u16, h: u16, vrefresh: u32) -> u32 {
        if let Some(id) = self.modes.get(&(w, h, vrefresh)) {
            return *id;
        }
        let id = self.fresh();
        self.modes.insert((w, h, vrefresh), id);
        id
    }
}
```

Add a field `randr_id_alloc: RandrIdAllocator` to `KmsBackendV2` and initialise it `RandrIdAllocator::default()` at every constructor site (mirror how `libinput_hotplug_retry_until: None` is initialised at `:799,:937,:1751`).

- [ ] **Step 4: Rewrite `randr_outputs`** (`:2359`) to use the allocator and return **sorted by ascending output_id** (stable primary, decision #4). It now takes `&mut self`:

```rust
    pub fn randr_outputs(&mut self) -> Vec<yserver_core::randr::RandrOutput> {
        use yserver_core::randr::RandrOutput;
        let mut outs: Vec<RandrOutput> = self
            .platform
            .outputs
            .iter()
            .map(|layout| {
                let vrefresh = layout.output.picked.vrefresh;
                let ids = self.randr_id_alloc.ids_for(&layout.output.connector_name);
                let mode_id = self.randr_id_alloc.mode_id(layout.width, layout.height, vrefresh);
                RandrOutput {
                    name: layout.output.connector_name.clone(),
                    output_id: ids.output_id,
                    crtc_id: ids.crtc_id,
                    mode_id,
                    x: i16::try_from(layout.x).unwrap_or(i16::MAX),
                    y: i16::try_from(layout.y).unwrap_or(i16::MAX),
                    width: layout.width,
                    height: layout.height,
                    vrefresh,
                    mm_width: layout.output.mm_width,
                    mm_height: layout.output.mm_height,
                }
            })
            .collect();
        outs.sort_by_key(|o| o.output_id);
        outs
    }
```

> NOTE: `randr_outputs` is also called at startup to build the initial `RandrState` (via `ServerState::with_randr_outputs`). Find that call site (it currently borrows `&self`) and make it `&mut`. The startup call seeds the allocator with the boot connectors — so boot ids and hotplug ids share one stable space. Verify the call chain compiles (the construction site may need `&mut backend`).

- [ ] **Step 5: Run → PASS.** `cargo test -p yserver --locked randr_ids 2>&1 | tail`.
- [ ] **Step 6: Build + fmt + clippy + commit.**

```bash
cargo build --locked 2>&1 | tail && cargo +nightly fmt && cargo clippy -p yserver --locked 2>&1 | grep -E "warning|error" | head
git add crates/yserver/src/kms/v2/backend.rs
git commit -m "feat(kms): stable connector-keyed RANDR id allocator (issue #9, finding #2)"
```

---

## Task 5b: Persist disconnected connectors in RANDR state (fixes finding #7)

**Files:** `crates/yserver-core/src/randr.rs`, `crates/yserver-core/src/core_loop/process_request.rs`, `crates/yserver/src/kms/v2/backend.rs` (`randr_outputs`)

Xorg keeps an `RROutput` resource per connector for the session and only flips its connection status, so a client that queries the output named in an `OutputChangeNotify` always resolves it. yserver currently only models *connected* outputs. This task makes `randr.outputs` hold every connector yserver has seen, with a `connected` flag.

> Scope note (documented deviation): connectors that have *never* been connected since server start are not listed (yserver enumerates only connected connectors via `discover_outputs`). Xorg lists all DRM connectors. Listing never-connected connectors would require a `discover_outputs`-level change and is deferred — it does not affect the issue-#9 hotplug/notify-then-query contract, which only concerns connectors that *were* present.

- [ ] **Step 1: Add `connected` to `RandrOutput`** (`randr.rs:7`). Add `pub connected: bool,` to the struct, then set `connected: true` at **every** existing literal (verified complete list — a missed one fails the build):
  - `crates/yserver-core/src/randr.rs`: `nested()` ctor (`:128`), `resize()` (`:162` — note `..out` spread already carries `connected`, but the literal sets `width`/`height`; confirm the spread covers `connected` so no edit needed there), and all `#[cfg(test)]` literals (`:351, :364, :391, :404, :425, :438, :458, :481, :504, :517`).
  - `crates/yserver-core/src/nested.rs:378`.
  - `crates/yserver-core/src/core_loop/process_request.rs:23565, :23578, :32931`.
  - `crates/yserver/src/kms/v2/backend.rs:2382` (the `randr_outputs` map — already touched in Task 5; set `connected: true` for the live entries).

- [ ] **Step 1b: Fix `from_outputs` for disconnected entries** (`randr.rs:72`). This is the load-bearing correctness fix (rev-3 codex caught two bugs here): a disconnected stub has `mode_id = 0` and `0×0` geometry, so (a) the mode-collection loop (`:92-106`) must SKIP it or it injects a bogus zero-mode into `state.modes` that `GetScreenResourcesCurrent` would advertise, and (b) `primary_output` must be the first **connected** output, not `outputs.first()` (which, sorted by id, could be a disconnected connector after a low-id monitor is unplugged):

```rust
        // Collect unique modes from CONNECTED outputs only (a
        // disconnected stub carries mode_id 0 / 0×0 — not a real mode).
        let mut modes: Vec<RandrMode> = Vec::new();
        let mut seen: HashSet<u32> = HashSet::new();
        for out in outputs.iter().filter(|o| o.connected) {
            if seen.insert(out.mode_id) {
                modes.push(RandrMode {
                    mode_id: out.mode_id, width: out.width,
                    height: out.height, vrefresh: out.vrefresh,
                });
            }
        }
        // Primary is the first CONNECTED output (fall back to first overall
        // only if nothing is connected — degenerate headless case).
        let primary_output = outputs
            .iter()
            .find(|o| o.connected)
            .or_else(|| outputs.first())
            .map_or(0, |o| o.output_id);
```

The `screen_width`/`screen_height` math already ignores 0×0 outputs (max of `x+width` / `height`), so no change there.

- [ ] **Step 2: Write the failing test** (`randr.rs` test module):

```rust
    #[test]
    fn disconnected_output_is_still_queryable() {
        let mut outs = vec![RandrOutput {
            name: "DP-1".into(), output_id: 1, crtc_id: 2, mode_id: 3,
            x: 0, y: 0, width: 2560, height: 1440, vrefresh: 60,
            mm_width: 0, mm_height: 0, connected: true,
        }];
        outs.push(RandrOutput {
            name: "HDMI-A-1".into(), output_id: 4, crtc_id: 5, mode_id: 0,
            x: 0, y: 0, width: 0, height: 0, vrefresh: 0,
            mm_width: 0, mm_height: 0, connected: false,
        });
        let st = RandrState::from_outputs(1, outs);
        // The disconnected output still resolves, reports crtc=0 + Disconnected.
        let info = st.output_info(4, 0).expect("disconnected output still queryable");
        assert_eq!(info.crtc, 0, "disconnected → no active crtc");
        assert_eq!(info.connection, 1, "1 = Disconnected");
        // Screen extent ignores the disconnected output (0x0 contributes nothing).
        assert_eq!(st.screen_width, 2560);
        // No bogus zero-mode injected by the disconnected stub.
        assert_eq!(st.modes.len(), 1, "only the connected output's mode");
        assert!(!st.modes.iter().any(|m| m.mode_id == 0), "no mode_id 0 advertised");
        // Primary is the CONNECTED output, never the disconnected one.
        assert_eq!(st.primary_output, 1);
    }

    #[test]
    fn primary_prefers_connected_when_lower_id_is_disconnected() {
        // DP-1 (id 1) unplugged, HDMI-A-1 (id 4) live → primary must be 4.
        let outs = vec![
            RandrOutput {
                name: "DP-1".into(), output_id: 1, crtc_id: 2, mode_id: 0,
                x: 0, y: 0, width: 0, height: 0, vrefresh: 0,
                mm_width: 0, mm_height: 0, connected: false,
            },
            RandrOutput {
                name: "HDMI-A-1".into(), output_id: 4, crtc_id: 5, mode_id: 6,
                x: 0, y: 0, width: 1920, height: 1080, vrefresh: 60,
                mm_width: 0, mm_height: 0, connected: true,
            },
        ];
        assert_eq!(RandrState::from_outputs(1, outs).primary_output, 4);
    }
```

- [ ] **Step 3: Run → FAIL** (`OutputInfoReplyData` has no `connection` field; `RandrOutput` has no `connected`).

- [ ] **Step 4: Thread `connection` through.**
  - Add `pub connection: u8,` to `OutputInfoReplyData` (`randr.rs:265`).
  - In `output_info` (`:215`), set `connection: if out.connected { 0 } else { 1 }`, and `crtc: if out.connected { out.crtc_id } else { 0 }`. A disconnected output's `mode_id` is 0; keep returning it.
  - In `screen_resources_current` (`:176`): include disconnected outputs in the `outputs`/`crtcs` arrays (Xorg lists them). It reads `self.modes` (already built by `from_outputs`), so once Step 1b filters disconnected entries out of `self.modes`, no zero-mode is advertised — no per-call filtering needed here.
  - Mode-injection + primary correctness live in `from_outputs` (Step 1b), NOT here — `screen_resources_current` only reflects what `from_outputs` already computed.

- [ ] **Step 5: Use the real connection in the handler.** In `process_request.rs:2204-2212`, replace the hardcoded `connection: 0` with `connection: info_data.connection`.

- [ ] **Step 6: Make `randr_outputs` emit all known connectors.** In `backend.rs`, change `randr_outputs` (Task 5 version) so that after building the connected entries from `platform.outputs`, it also appends a disconnected stub for every name in `self.randr_id_alloc` that is NOT currently in `platform.outputs`:

```rust
        // Append disconnected stubs for connectors we've seen but that
        // are no longer present, so clients can still query the id named
        // in an OutputChangeNotify (Xorg keeps the output resource).
        let live: std::collections::HashSet<&str> =
            self.platform.outputs.iter().map(|l| l.output.connector_name.as_str()).collect();
        let known: Vec<(String, ConnectorIds)> = self
            .randr_id_alloc
            .known_connectors(); // (name, ids) snapshot — add this accessor
        for (name, ids) in known {
            if !live.contains(name.as_str()) {
                outs.push(RandrOutput {
                    name, output_id: ids.output_id, crtc_id: ids.crtc_id, mode_id: 0,
                    x: 0, y: 0, width: 0, height: 0, vrefresh: 0,
                    mm_width: 0, mm_height: 0, connected: false,
                });
            }
        }
        outs.sort_by_key(|o| o.output_id);
        outs
```

Add `RandrIdAllocator::known_connectors(&self) -> Vec<(String, ConnectorIds)>` returning a snapshot of the `outputs` map.

- [ ] **Step 7: Run → PASS.** `cargo test -p yserver-core --locked "disconnected_output|randr" 2>&1 | tail -30`. Fix any existing `RandrOutput` test literals broken by the new field.
- [ ] **Step 8: Build + fmt + clippy + commit.**

```bash
cargo build --locked 2>&1 | tail && cargo +nightly fmt && cargo clippy --locked 2>&1 | grep -E "warning|error" | head
git add crates/yserver-core/src/randr.rs crates/yserver-core/src/core_loop/process_request.rs crates/yserver/src/kms/v2/backend.rs
git commit -m "feat(randr): persist disconnected connectors so notify ids stay queryable (issue #9, finding #7)"
```

---

## Task 6: Scene-side output rebuild after GPU drain (fixes finding #1 — critical)

**Files:** `crates/yserver/src/kms/v2/scene.rs`

`OutputSceneState` holds only transient GPU/damage/cursor state (no persistent client data), so after a full GPU drain the whole `inner.outputs` vector can be rebuilt from `platform.outputs` safely. This avoids index-renumbering and in-flight-fence-free hazards entirely (decision #6).

- [ ] **Step 1: Add `SceneCompositor::rebuild_outputs`.** Factor the per-output construction (`scene.rs:566-599`) into a private `fn build_output_state(vk, platform, i) -> Result<OutputSceneState, SceneError>` and call it from both `new` and the new rebuild:

```rust
    /// Rebuild the per-output scene-state vector to match
    /// `platform.outputs` after a DRM hotplug (issue #9). The CALLER
    /// MUST have already FULLY quiesced the GPU — `platform.wait_idle_
    /// bounded()` (device_wait_idle) THEN `self.drain_all(platform)` —
    /// so dropping the old `OutputSceneState`s (incl. any
    /// `failed_submit_bos` still holding fence tickets) frees nothing
    /// the GPU is still reading. `drain_all` alone is NOT sufficient: it
    /// does not wait `failed_submit_bos` (finding #6). Cursor sprite +
    /// pipeline are preserved. No-op if the scene has no inner (Vk-less
    /// test fixture).
    pub(crate) fn rebuild_outputs(
        &mut self,
        platform: &PlatformBackend,
    ) -> Result<(), SceneError> {
        let Some(inner) = self.inner.as_mut() else { return Ok(()) };
        let vk = inner.vk.clone();
        let mut outputs = Vec::with_capacity(platform.outputs.len());
        for i in 0..platform.outputs.len() {
            outputs.push(Self::build_output_state(&vk, platform, i)?);
        }
        inner.outputs = outputs;
        inner.deferred_upload_wait_set.clear(); // indices invalidated by rebuild
        self.scene_structure_dirty = true;       // force a full scene rebuild next tick
        log::info!("scene: rebuilt {} output state(s) after topology change", platform.outputs.len());
        Ok(())
    }
```

> Implement `build_output_state` by lifting lines `566-598` verbatim (the `bo_depth` lookup + the full `OutputSceneState { .. }` literal), parameterised on `i` and `platform`. Confirm `inner.vk` is the field name (per scene.rs:470); adjust if different.

- [ ] **Step 2: Add a lockstep guard.** In `tick()` (or `tick_one_output`), add a debug assertion so any future desync fails loudly in tests/debug rather than via a raw `expect("range")` panic in production:

```rust
        debug_assert_eq!(
            inner.outputs.len(),
            platform.outputs.len(),
            "scene/platform output vectors must stay in lockstep (issue #9)"
        );
```

- [ ] **Step 3: Build + a length test on the fixture.** The Vk-less `for_tests` path makes `rebuild_outputs` a no-op (inner is None), so a meaningful unit test is limited; assert it does not error and is a no-op:

```rust
    #[test]
    fn rebuild_outputs_is_noop_without_vk() {
        let mut scene = SceneCompositor::without_vk_for_tests(); // reuse existing ctor if present
        let platform = PlatformBackend::for_tests();
        assert!(scene.rebuild_outputs(&platform).is_ok());
    }
```

> If no Vk-less scene ctor exists, skip this unit test and rely on the lockstep `debug_assert` + the HW gate; note that in the commit message. Do not fabricate a fixture.

- [ ] **Step 4: Build + fmt + clippy + commit.**

```bash
cargo build --locked 2>&1 | tail && cargo +nightly fmt && cargo clippy -p yserver --locked 2>&1 | grep -E "warning|error" | head
git add crates/yserver/src/kms/v2/scene.rs
git commit -m "feat(kms): scene-side output rebuild after GPU drain (issue #9, finding #1)"
```

---

## Task 7: Orchestrate the topology change (rewrite fire_randr_changes)

**Files:** `crates/yserver/src/kms/v2/backend.rs`

`fire_randr_changes` becomes the single place that, given a `RescanResult` (platform already re-probed), brings the scene + RANDR state + clients into agreement. Called by both `run_resume` (Task 4 Step 5) and the debounced hotplug path (Task 8).

- [ ] **Step 1: Rewrite `fire_randr_changes`** (`:4807`). Order is load-bearing: full GPU quiesce → scene rebuild (FATAL on failure, before any state divergence is observable) → RANDR rebuild → per-output emit (read from the now-persistent `randr.outputs`, which includes disconnected entries) → repaint.

```rust
    fn fire_randr_changes(
        &mut self,
        state: &mut ServerState,
        rescan: crate::kms::v2::platform::RescanResult,
    ) {
        for n in &rescan.added_names { log::info!("kms: RandR output connected: {n}"); }
        for n in &rescan.dropped_names { log::info!("kms: RandR output disconnected: {n}"); }

        // 1. FULL GPU quiesce — device_wait_idle (waits failed_submit_bos
        //    too, finding #6) THEN drain_all (releases descriptor slots).
        //    `platform.outputs` was already mutated by the re-probe; the
        //    scene's parallel vector is now stale and MUST be rebuilt
        //    before the next tick indexes it.
        self.platform.wait_idle_bounded();
        self.scene.drain_all(&mut self.platform);

        // 2. Rebuild the scene's per-output vector in lockstep. A failure
        //    here leaves scene/platform desynced and the next tick would
        //    panic on expect("range"); treat it as fatal, exactly like
        //    resume's card-gone path (finding #8). request_exit queues a
        //    clean Shutdown — no half-applied transition keeps running.
        if let Err(e) = self.scene.rebuild_outputs(&self.platform) {
            log::error!("kms: scene rebuild after topology change failed: {e:?}; exiting");
            self.request_exit();
            return;
        }

        // 3. Rebuild the RANDR source-of-truth. `randr_outputs` now
        //    returns connected outputs (stable ids, sorted) PLUS
        //    disconnected stubs for previously-seen connectors (Task 5b),
        //    so every id we announce stays queryable.
        let timestamp = state.timestamp_now();
        let outputs = self.randr_outputs();
        state.randr = yserver_core::randr::RandrState::from_outputs(timestamp, outputs);

        // 4. Keep root + overlay geometry in agreement with the new extent.
        let (w, h) = (state.randr.screen_width, state.randr.screen_height);
        if let Some(root) = state.resources.window_mut(yserver_core::resources::ROOT_WINDOW) {
            root.width = w; root.height = h;
        }
        if let Some(overlay) =
            state.resources.window_mut(yserver_core::resources::COMPOSITE_OVERLAY_WINDOW)
        {
            overlay.width = w; overlay.height = h;
        }

        // 5. Per-output fanout over EVERY output now in randr (connected +
        //    disconnected) so OutputChangeNotify names ids clients can
        //    still query, then full repaint.
        let changed: Vec<(u32, u32, u32)> = state
            .randr
            .outputs
            .iter()
            .map(|o| (o.output_id, o.crtc_id, o.mode_id))
            .collect();
        yserver_core::core_loop::run::emit_randr_change_notifications(state, &changed);
        self.scene.wake_for_damage();
    }
```

> `emit_randr_change_notifications` is `pub` (Task 3). Confirm the `yserver` crate can reach it; if the module path differs, add `pub use run::emit_randr_change_notifications;` to `core_loop/mod.rs` and adjust the call. `wait_idle_bounded()` is `platform.rs:2166` (the 5 s-bounded `device_wait_idle` the suspend path uses at `backend.rs:4621`). `request_exit()` is `backend.rs:4795`.

- [ ] **Step 2: Build + tests + commit.**

```bash
cargo build --locked 2>&1 | tail && cargo test --locked 2>&1 | tail -20 && cargo +nightly fmt
git add crates/yserver/src/kms/v2/backend.rs crates/yserver-core/src/core_loop/run.rs crates/yserver-core/src/core_loop/mod.rs
git commit -m "feat(kms): orchestrate scene+RANDR rebuild on topology change (issue #9)"
```

---

## Task 8: Monitor wiring + debounced on_display_hotplug (fixes finding #4)

**Files:** `crates/yserver/src/kms/v2/platform.rs`, `crates/yserver/src/kms/v2/backend.rs`

- [ ] **Step 1: Add + construct the monitor.** In `PlatformBackend` add `#[cfg(target_os = "linux")] pub(crate) hotplug_monitor: Option<crate::kms::hotplug::DrmHotplugMonitor>,`. Initialise in `open_with_commit`/`_fd` (`:813`) with `crate::kms::hotplug::DrmHotplugMonitor::new().unwrap_or(None)`; `None` in `for_tests` (`:862`).

- [ ] **Step 2: Register in `poll_fds`** (`:1254`), after the existing pushes:

```rust
        #[cfg(target_os = "linux")]
        if let Some(mon) = self.hotplug_monitor.as_ref() {
            fds.push((mon.raw_fd(), BackendFdKind::DrmHotplug));
        }
```

- [ ] **Step 3: Add the debounce field.** Add `hotplug_rescan_deadline: Option<std::time::Instant>` to `KmsBackendV2`, initialised `None` at every constructor (mirror `libinput_hotplug_retry_until`).

- [ ] **Step 4: Write the failing debounce test** (mirror `hotplug_retry_window_arms_next_wakeup_cadence` at `:15997`):

```rust
    #[test]
    fn display_hotplug_arms_rescan_deadline_and_next_wakeup() {
        let mut b = KmsBackendV2::for_tests();
        assert!(b.next_wakeup().is_none());
        // Simulate a hotplug edge by arming the deadline directly (the
        // udev drain is HW-only; this exercises the debounce/timer wiring).
        b.hotplug_rescan_deadline =
            Some(std::time::Instant::now() + std::time::Duration::from_millis(150));
        assert!(b.next_wakeup().is_some(), "armed rescan deadline must drive next_wakeup");
    }
```

- [ ] **Step 5: Implement debounce in `on_display_hotplug` + service in `poll_deferred_input` + chain in `next_wakeup`.**

`on_display_hotplug` (only arms the timer — no heavy work on the poll thread):

```rust
    fn on_display_hotplug(&mut self, _state: &mut ServerState) {
        #[cfg(target_os = "linux")]
        {
            let saw_change = self.platform.hotplug_monitor.as_mut().map(|m| m.drain()).unwrap_or(false);
            if saw_change {
                // Coalesce a udev burst into one re-probe 150 ms out.
                self.hotplug_rescan_deadline =
                    Some(std::time::Instant::now() + std::time::Duration::from_millis(150));
                log::debug!("kms: display hotplug edge — rescan armed (+150ms)");
            }
        }
    }
```

Extend `poll_deferred_input` (`:9522`) to also service the rescan deadline (after the existing libinput-window block):

```rust
        if let Some(deadline) = self.hotplug_rescan_deadline {
            if std::time::Instant::now() >= deadline {
                self.hotplug_rescan_deadline = None;
                self.run_display_rescan(state);
            }
        }
```

Add the worker (the heavy path, now off the readiness edge):

```rust
    /// Debounced connector re-probe + scene/RANDR rebuild (issue #9).
    fn run_display_rescan(&mut self, state: &mut ServerState) {
        // Don't re-probe while VT-away / suspended — resume handles that.
        if self.core_libinput.is_some()
            && self.seat_state != crate::seat::state::SeatState::Active
        {
            log::debug!("kms: display rescan skipped (seat not Active)");
            return;
        }
        log::info!("kms: display rescan — re-probing connectors");
        match self.platform.requery_outputs_and_modeset() {
            Ok(rescan) => {
                if rescan.added_count == 0 && rescan.dropped_names.is_empty() {
                    log::debug!("kms: rescan found no topology change");
                    return;
                }
                // DPMS-aware: only assert outputs active when powered On.
                if state.dpms.power_level == 0 {
                    self.kms_outputs_active = true;
                }
                self.fire_randr_changes(state, rescan);
            }
            Err(e) => log::error!("kms: display rescan failed (card gone?): {e}"),
        }
    }
```

Chain the deadline in `next_wakeup` (`:8957-8967`), adding alongside `hotplug_retry_deadline`:

```rust
        let rescan_deadline = self.hotplug_rescan_deadline;
        // ... .chain(hotplug_retry_deadline).chain(rescan_deadline) ...
```

> Verify `state.dpms.power_level == 0` is the "On" sentinel (per the DPMS code `0=On`); adjust the constant/path if the field differs. Verify `SeatState::Active` path.

- [ ] **Step 6: Run debounce + poll_fds tests → PASS.** `cargo test -p yserver --locked "hotplug|poll_fds" 2>&1 | tail`.
- [ ] **Step 7: Full suite + fmt + clippy.** `cargo test --locked 2>&1 | tail -20 && cargo +nightly fmt && cargo clippy --locked 2>&1 | grep -E "warning|error" | head`.
- [ ] **Step 8: Commit.**

```bash
git add crates/yserver/src/kms/v2/platform.rs crates/yserver/src/kms/v2/backend.rs
git commit -m "feat(kms): debounced display rescan off the poll thread (issue #9, finding #4)"
```

---

## Task 9: Hardware smoke test (acceptance gate — `silence`, multi-head)

**Unit-green is NOT a HW pass.** Issue #9 was observed on `silence` under lightdm. Per memory: HW runs go through the `just startx`/tmux procedure, ONE agent per checkout — coordinate before running.

- [ ] **Step 1: Build + launch on a free VT with ONE monitor.** `just startx`. Confirm the log enumerates one output.
- [ ] **Step 2: Power on / connect a second monitor.** Expect log: `drm hotplug: udev ... action="change"` → `kms: display hotplug edge — rescan armed` → `kms: display rescan — re-probing` → `kms: RandR output connected: <CONNECTOR>` → `scene: rebuilt 2 output state(s)` → new fb extent. On screen: second monitor lights with the desktop (no longer blank).
- [ ] **Step 3: `xrandr --query`** — newly-connected output appears connected with its mode; screen dims reflect the extended layout; **note the output IDs**.
- [ ] **Step 4: Hotplug-remove, then re-add.** Expect `RandR output disconnected`, `xrandr` shows it gone, primary keeps working, no crash/panic. Re-add and confirm via `xrandr` that the **same output ID** comes back (validates Task 5's stable-ID invariant on real HW).
- [ ] **Step 5: Stress the debounce.** Power-cycle the monitor a few times quickly; confirm exactly one `display rescan` per settle (coalesced), input/cursor stays responsive throughout (validates Task 8 — no poll-thread stall).
- [ ] **Step 6: Record results** (with exact log lines) in the PR description. If the second monitor stays blank with no `display hotplug` log, debug `DrmHotplugMonitor::drain`'s action filter against `udevadm monitor --subsystem-match=drm` before suspecting the re-probe.
- [ ] **Step 7 (if green): open the PR** — push branch, draft body, **ask before publishing** (global rule: no public text in the user's name without approval). `Closes #9`.

---

## Self-review

- **Findings → fixes:**
  - rev 1: #1 scene desync → Task 6 + Task 7 + lockstep `debug_assert`. #2 ID renumbering → Task 5 (connector-keyed allocator) + sorted-by-id primary. #3 wrong output in events → Task 3 (per-output emit). #4 debounce/poll-stall/DPMS clobber → Tasks 2 + 8. #5 tautological tests → Task 4 (issue-#9 `5120x1440`), Task 5 (stable-ID invariant), Task 9 step 4 (re-add same ID on HW).
  - rev 2: #6 incomplete quiesce → Task 7 uses `wait_idle_bounded()` (full `device_wait_idle`) then `drain_all`. #7 dropped outputs unqueryable → Task 5b (persistent connectors with `connected` flag + real `connection` byte). #8 non-transactional rebuild → Task 7 makes scene-rebuild failure fatal (`request_exit`), no desynced-and-running state.
  - rev 3: #9 phantom zero-mode → Task 5b Step 1b filters the `from_outputs` mode loop on `connected`. #10 primary could be disconnected → Step 1b prefers first connected. #11 missing `connected` literals → Step 1 lists all sites.
- **Type consistency:** `requery_outputs_and_modeset → RescanResult` (T4) consumed by `fire_randr_changes(state, RescanResult)` (T7), called by `run_resume` (T4.5) and `run_display_rescan` (T8). `RandrOutput` gains `connected: bool` (T5b) — every literal updated. `randr_outputs(&mut self)` uses `RandrIdAllocator` + `known_connectors()` (T5/T5b), returns connected + disconnected stubs sorted by id. `OutputInfoReplyData.connection` (T5b) consumed at `process_request.rs:2204`. `emit_randr_change_notifications(state, &[(u32,u32,u32)])` (T3) reads its list from `randr.outputs` in T7. `SceneCompositor::rebuild_outputs(&platform)` (T6) called from T7 after full quiesce.
- **Implementer MUST verify against the live tree (don't trust the plan blindly):** `udev::MonitorBuilder` 0.9 API; `SeatState::Active` path + `seat_state`; `state.dpms.power_level` On-sentinel; `inner.vk` field name in scene; `emit_randr_change_notifications` reachability/re-export; `randr_outputs` startup call site (`ServerState::with_randr_outputs`) now needs `&mut backend`; all `RandrOutput` literals across `randr.rs` tests get `connected: true`; `screen_resources_current` listing disconnected outputs must not break existing xts/RANDR fixtures that count outputs — if a fixture asserts a fixed output count, confirm it only runs with all-connected state; run.rs/scene/backend test-helper names (reuse existing, don't invent); `dropped_old_indices` is informational only (rebuild is wholesale, doesn't replay indices).
- **Deferred (not stubs):** FreeBSD KMS hotplug (no udev; `#[cfg]`); never-connected connectors not listed in GetScreenResources (Task 5b scope note); mirror-mode + explicit placement (extend-right default); XINERAMA (sibling issue).
