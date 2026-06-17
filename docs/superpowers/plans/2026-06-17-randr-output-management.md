# RANDR Output Management (mode-set + layout) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement client-driven RANDR output management (`RRSetCrtcConfig`, `RRSetScreenSize`, full mode lists, off/disconnected reporting) so a real DE (MATE) owns multi-monitor layout on the KMS backend, matching Xorg.

**Architecture:** `yserver-core` owns RANDR protocol state, validation, replies, and notify fanout (unit-testable, no GPU). The KMS backend (`yserver`) owns DRM apply behind three new `Backend` trait methods (`apply_crtc_config`, `set_logical_screen_size`, `reprobe_connectors`). A **connector registry** — grown from the existing `RandrIdAllocator` — is the authoritative store for each connector's mode list, connection state, current config, and a `client_configured` flag that stops the rescan/resume auto-layout from flattening a client's geometry.

**Tech Stack:** Rust (stable toolchain), DRM/KMS atomic modeset (`drm` crate), Vulkan scanout (ash), `cargo test` for core unit tests, HW acceptance on fuji/MATE.

**Spec:** `docs/superpowers/specs/2026-06-17-randr-output-management-design.md`. Xorg de-facto spec at `/home/jos/Projects/xserver/randr/`. Stacks on `feat/drm-hotplug`.

---

## Orientation: key files and current state

| File | Role | Key locations |
|---|---|---|
| `crates/yserver-core/src/randr.rs` | `RandrState`, `RandrOutput`, `RandrMode`, reply builders | `from_outputs` :73 (aggregation :79–91); `RandrOutput` :7; `output_info` :221; `crtc_info` :260 |
| `crates/yserver-core/src/core_loop/process_request.rs` | RANDR request dispatch + handlers | dispatch `handle_randr_request` :2117; GetScreenResources :2167; GetOutputInfo :2178; GetCrtcInfo :2226; GetMonitors :2341; `active_monitors` :2087; the **no-op** SetCrtcConfig/SetScreenConfig :2454; XINERAMA QueryScreens :2968; confine clamp :22322 |
| `crates/yserver-core/src/core_loop/run.rs` | resize fanout helpers | `handle_host_container_resize` :859; `emit_randr_change_notifications` :929 |
| `crates/yserver-core/src/backend/trait_def.rs` | `Backend` trait | trait starts :286; loop hooks :310–370; `warp_pointer_root` :1851 |
| `crates/yserver-core/src/backend/recording.rs` | test-double backend | `impl Backend for RecordingBackend` :253 |
| `crates/yserver-core/src/server.rs` | `ServerState` | `randr` field :734; `pointer_root` :750 |
| `crates/yserver/src/kms/v2/backend.rs` | KMS v2 backend + `RandrIdAllocator` | `RandrIdAllocator` :140; `ConnectorIds` :147; `randr_outputs` :2416; `fire_randr_changes` :4889 (rebuilds `state.randr` :4911) |
| `crates/yserver/src/kms/v2/platform.rs` | DRM/Vk platform surface | `recompute_fb_extent_from` :623; `recompact_horizontal_layout` :2351; `requery_outputs_and_modeset` :2363; `dpms_set_outputs_active` :2285; `wait_idle_bounded` :2202; `outputs`/`fb_w`/`fb_h` fields :504+ |
| `crates/yserver/src/kms/backend.rs` | shared KMS types | `OutputLayout` :390 |
| `crates/yserver/src/drm/modeset.rs` | DRM discovery/modeset | `Mode` :16; `local_mode_from` :65; `Output` :76; `discover_outputs` :193 (`local_modes` :301); `disable_output` :516; `commit_modeset` :537 |
| `crates/yserver/src/kms/v2/scene.rs` | scene compositor | `rebuild_outputs` :620; `drain_all` :854; per-output offset compose :1902 |

**Current data flow (boot/hotplug):** `requery_outputs_and_modeset` discovers outputs → `recompact_horizontal_layout` (forces `y=0`, extend-right) → `fire_randr_changes` → `randr_outputs()` builds `Vec<RandrOutput>` (enabled from `platform.outputs`, disconnected from `RandrIdAllocator::known_connectors`) → `state.randr = RandrState::from_outputs(...)`.

**Build/test commands** (run from repo root):
- Core unit tests: `cargo test -p yserver-core randr` and `cargo test -p yserver-core`
- Full workspace build: `cargo build --locked`
- Pre-commit gate (per CLAUDE.md): `cargo fmt`, `cargo clippy`, `cargo test`
- HW acceptance: fuji/MATE via `just startx` (see §HW acceptance gate). HW runs are user-only — coordinate per memory `feedback_hw_recipes_user_only`.

**Branching:** all work lands as one stacked set on `feat/drm-hotplug` (already the current branch). Commit per task.

---

## Phase 1 — Mode lists, connector registry, 2-D bounding box

Goal: every connector carries its full mode list (preferred-first); `RandrState`/`RandrOutput` carry mode-id lists and a logical screen size; bounding-box math becomes 2-D. No behavior change to enable/disable yet.

### Task 1.1: Add `modes` to the DRM `Output` struct

**Files:**
- Modify: `crates/yserver/src/drm/modeset.rs:76` (struct), `:301` (stop discarding `local_modes`)

- [ ] **Step 1: Add the field to `Output`**

In `crates/yserver/src/drm/modeset.rs`, add to the `Output` struct (after `mm_height`, around line 104):

```rust
    /// EDID-derived physical height; see [`Self::mm_width`].
    pub mm_height: u32,
    /// The connector's full local mode list, preferred-first, as
    /// reported by the kernel/EDID. `picked` is the boot default and
    /// is always present in this list. Used by RANDR to advertise the
    /// selectable mode set (`GetOutputInfo` / `GetScreenResources`) and
    /// by `apply_crtc_config` to resolve a client-requested mode.
    pub modes: Vec<Mode>,
```

- [ ] **Step 2: Populate `modes` in `discover_outputs`**

In `discover_outputs`, `local_modes` is computed (line ~301) and currently only used to pick the boot mode. Sort it preferred-first and store it on the finalized `Output`. Find where each `Output { .. }` is constructed (the finalize block, lines ~285–291) and add `modes:`. First, make `local_modes` preferred-first right after it is computed:

```rust
    let mut local_modes: Vec<Mode> =
        connector_info.modes().iter().map(local_mode_from).collect();
    // Preferred-first, matching Xorg GetOutputInfo (nPreferred prefix).
    local_modes.sort_by_key(|m| !m.preferred);
```

Then in the `Output { .. }` construction add the field (clone, since `local_modes` may be moved/used for `pick_mode`):

```rust
        modes: local_modes.clone(),
```

(If `local_modes` is consumed by `pick_mode` before the struct literal, reorder so the struct gets the clone first, or pass `&local_modes` to `pick_mode`. Keep `picked` exactly as today.)

- [ ] **Step 3: Build to verify it compiles**

Run: `cargo build -p yserver --locked 2>&1 | tail -20`
Expected: compiles (every `Output { .. }` literal now has `modes:`; if any other construction site exists, the compiler will name it — add the field there too).

- [ ] **Step 4: Commit**

```bash
git add crates/yserver/src/drm/modeset.rs
git commit -m "feat(randr): retain full per-connector mode list on Output

Stops discarding local_modes in discover_outputs; sorts preferred-first.
Groundwork for client-driven RANDR mode selection.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

### Task 1.2: 2-D bounding box in `RandrState::from_outputs` (failing test first)

**Files:**
- Modify: `crates/yserver-core/src/randr.rs:79-91` (aggregation) and doc comment :66-71
- Test: `crates/yserver-core/src/randr.rs` (existing `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `crates/yserver-core/src/randr.rs`:

```rust
    #[test]
    fn from_outputs_screen_height_is_2d_for_vertical_stack() {
        // External monitor stacked BELOW the laptop panel: second
        // output at y=1080. screen_height must be 1080+1080=2160, not
        // max(height)=1080. (Spec: screen_height = max(y+height).)
        let outs = vec![
            RandrOutput {
                name: "eDP-1".into(),
                output_id: 1,
                crtc_id: 2,
                mode_id: 3,
                connected: true,
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                vrefresh: 60,
                mm_width: 0,
                mm_height: 0,
                mode_ids: vec![3],
                num_preferred: 1,
            },
            RandrOutput {
                name: "HDMI-A-1".into(),
                output_id: 4,
                crtc_id: 5,
                mode_id: 6,
                connected: true,
                x: 0,
                y: 1080,
                width: 1920,
                height: 1080,
                vrefresh: 60,
                mm_width: 0,
                mm_height: 0,
                mode_ids: vec![6],
                num_preferred: 1,
            },
        ];
        let st = RandrState::from_outputs(0, outs);
        assert_eq!(st.screen_width, 1920);
        assert_eq!(st.screen_height, 2160, "screen must encompass y+height");
    }
```

This test also forces the `RandrOutput` field additions from Task 1.3 — do Task 1.3 Step 1 (the field additions) together with this so the test compiles.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p yserver-core from_outputs_screen_height_is_2d -- --nocapture`
Expected: FAIL — `assert_eq!(st.screen_height, 2160)` gets `1080` (current code: `outputs.iter().map(|o| o.height).max()`).

- [ ] **Step 3: Make `screen_height` 2-D**

In `from_outputs` (`randr.rs:87`), replace:

```rust
        let screen_height: u16 = outputs.iter().map(|o| o.height).max().unwrap_or(0);
```

with:

```rust
        let screen_height: u16 = outputs
            .iter()
            .map(|o| {
                let r = i32::from(o.y).saturating_add(i32::from(o.height));
                u16::try_from(r.max(0)).unwrap_or(u16::MAX)
            })
            .max()
            .unwrap_or(0);
```

Update the doc comment at `:68-69` from `screen_height = max(output.height)` to `screen_height = max(output.y + output.height)` and drop the "y is 0" note.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p yserver-core from_outputs_screen_height_is_2d`
Expected: PASS. Also run `cargo test -p yserver-core randr` — existing extent tests (`from_outputs_aggregates_screen_extent` etc., all `y=0`) still pass.

- [ ] **Step 5: Commit** (bundled with Task 1.3 — see 1.3 Step 6).

### Task 1.3: Add `mode_ids` + `num_preferred` to `RandrOutput`; logical screen size to `RandrState`

**Files:**
- Modify: `crates/yserver-core/src/randr.rs` — `RandrOutput` :7, `RandrState` :38, `from_outputs` :73, `nested` :132, all test fixtures
- Modify: `crates/yserver/src/kms/v2/backend.rs:2416` (`randr_outputs` — populate new fields)
- Modify: `crates/yserver-core/src/nested.rs` and `process_request.rs:32948` if they construct `RandrOutput`

- [ ] **Step 1: Add fields to `RandrOutput`**

In `crates/yserver-core/src/randr.rs`, add to `RandrOutput` (after `mm_height`):

```rust
    pub mm_height: u32,
    /// Available mode ids for this output, preferred-first. Empty for a
    /// disconnected output. The current mode is `mode_id` (0 = off).
    pub mode_ids: Vec<u32>,
    /// Count of leading entries in `mode_ids` that are preferred modes
    /// (Xorg `GetOutputInfo` `nPreferred`).
    pub num_preferred: u16,
```

- [ ] **Step 2: `screen_width`/`screen_height` ARE the logical/reported size (no separate field)**

> **Fix (codex Finding C):** the X11 protocol has exactly **one** screen size — the framebuffer/logical size the client owns via `RRSetScreenSize`, and the value `GetScreenResources`/`ScreenChangeNotify`/the bounds check all use (Xorg `pScreen->width/height`). Do **not** add a separate `logical_width`/`logical_height` that diverges from the reported `screen_width`/`screen_height` — that split caused the no-op-reprobe collapse bug. Instead: `screen_width`/`screen_height` ARE the logical size. `from_outputs` seeds them from the bounding box (boot default); `RRSetScreenSize` overrides them; the rebuild helper (Task 2.2 Step 1) **carries them forward** so a re-probe never resets a client-set size.

So **no new size fields** are added to `RandrState` here. Keep `screen_width`/`screen_height`/`width_mm`/`height_mm` as-is; their *meaning* becomes "the current logical screen size" rather than "the raw bounding box". The bounding box is computed transiently only when seeding the boot default (`from_outputs`) — it is not stored. Validation (`SetCrtcConfig` encompass, `SetScreenSize` crop) reads `screen_width`/`screen_height` directly.

(The doc comments on `screen_width`/`screen_height` at `randr.rs:47-50` already say "Aggregated virtual-screen extent"; update them to note this is the boot default and that `RRSetScreenSize` overrides it.)

- [ ] **Step 3: Build the per-output union of modes in `from_outputs`**

Today `from_outputs` dedups modes by `mode_id` across outputs that share `mode_id`. With per-output mode lists, the `modes` union must collect every `(mode_id, w, h, vrefresh)` referenced by any output's `mode_ids`. Since the core does not know each mode's geometry beyond the current one, the backend must supply geometry — see Step 5. For now keep the existing current-mode dedup loop (it still produces the modes actually in use) and extend it in Task 2.1 when `GetScreenResources` needs the full union. Leave the loop as-is for this task.

- [ ] **Step 4: Fix all `RandrOutput` literals**

Compile to find every construction site:

Run: `cargo build -p yserver-core --locked 2>&1 | rg "missing.*mode_ids|missing.*num_preferred" | head`

Add `mode_ids` + `num_preferred` to:
- `RandrState::nested` (`randr.rs:133`): `mode_ids: vec![3], num_preferred: 1,`
- Every test fixture in `randr.rs` `mod tests` (connected ones get `mode_ids: vec![<their mode_id>], num_preferred: 1`; the disconnected fixtures in `disconnected_output_is_still_queryable` and `primary_prefers_connected_*` get `mode_ids: vec![], num_preferred: 0`).
- `crates/yserver-core/src/nested.rs:394` synthetic output.
- `crates/yserver-core/src/core_loop/process_request.rs:32948` if it builds outputs (check the surrounding code).

- [ ] **Step 5: Populate the new fields in the KMS `randr_outputs()` bridge**

In `crates/yserver/src/kms/v2/backend.rs:2416` (`randr_outputs`), for each **enabled** output build its `mode_ids` from the connector's mode list (Task 1.1 added `output.modes`), allocating a stable mode id per `(w,h,vrefresh)` via `self.randr_id_alloc.mode_id(...)`, preferred-first, and count the leading preferred:

```rust
        for layout in &self.platform.outputs {
            let vrefresh = layout.output.picked.vrefresh;
            let ids = self.randr_id_alloc.ids_for(&layout.output.connector_name);
            let mode_id = self
                .randr_id_alloc
                .mode_id(layout.width, layout.height, vrefresh);
            // Full advertised list, preferred-first (Output.modes is
            // already sorted preferred-first by discover_outputs).
            let mut mode_ids = Vec::with_capacity(layout.output.modes.len());
            let mut num_preferred: u16 = 0;
            for m in &layout.output.modes {
                mode_ids.push(self.randr_id_alloc.mode_id(m.width, m.height, m.vrefresh));
                if m.preferred {
                    num_preferred = num_preferred.saturating_add(1);
                }
            }
            outs.push(RandrOutput {
                name: layout.output.connector_name.clone(),
                output_id: ids.output_id,
                crtc_id: ids.crtc_id,
                mode_id,
                connected: true,
                x: i16::try_from(layout.x).unwrap_or(i16::MAX),
                y: i16::try_from(layout.y).unwrap_or(i16::MAX),
                width: layout.width,
                height: layout.height,
                vrefresh,
                mm_width: layout.output.mm_width,
                mm_height: layout.output.mm_height,
                mode_ids,
                num_preferred,
            });
        }
```

For the **disconnected** branch (the `known_connectors` loop), set `mode_ids: vec![], num_preferred: 0,`. (Phase 5 will preserve the last-known mode list across disconnect; for now disconnected = empty list, matching the current behavior where they report `mode_id=0`.)

- [ ] **Step 6: Build + run all randr tests, then commit Tasks 1.2 + 1.3**

Run: `cargo build --locked 2>&1 | tail -5 && cargo test -p yserver-core randr`
Expected: clean build, all randr tests pass (including the new 2-D test).

```bash
git add crates/yserver-core/src/randr.rs crates/yserver-core/src/nested.rs crates/yserver-core/src/core_loop/process_request.rs crates/yserver/src/kms/v2/backend.rs
git commit -m "feat(randr): per-output mode-id lists + 2-D bbox

RandrOutput carries mode_ids (preferred-first) and num_preferred.
screen_width/height become the reported (logical) screen size, seeded
from the bounding box; screen_height is now max(y+height) so
vertically-stacked outputs are encompassed.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

### Task 1.4: Grow `RandrIdAllocator` into a connector registry

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs:140` (`RandrIdAllocator`, `ConnectorIds`)

The registry is the authoritative store the rescan/resume preservation rule and `apply_crtc_config` key off. Per spec it holds: stable ids (already), full mode list, connection state, current config (`Off`/`Enabled{mode,x,y}`), and `client_configured`.

- [ ] **Step 1: Add registry types and per-connector entry**

In `crates/yserver/src/kms/v2/backend.rs`, replace the `ConnectorIds`/`RandrIdAllocator` pair with an extended registry. Keep `ConnectorIds` (other code reads it) and add a richer entry:

```rust
/// Per-connector current configuration in the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectorConfig {
    /// Connected but not scanning out (mode=None, no CRTC).
    Off,
    /// Scanning out at `(mode_w, mode_h, vrefresh)` placed at `(x, y)`.
    Enabled {
        mode_w: u16,
        mode_h: u16,
        vrefresh: u32,
        x: i32,
        y: i32,
    },
}

/// One connector the backend has ever seen.
#[derive(Debug, Clone)]
pub(crate) struct ConnectorEntry {
    pub ids: ConnectorIds,
    pub connected: bool,
    pub config: ConnectorConfig,
    /// `true` once a client SetCrtcConfig/SetScreenSize touched this
    /// output. The auto-layout (recompact / boot extend-right) only
    /// ever touches `!client_configured` outputs; cleared on disconnect.
    pub client_configured: bool,
    /// Last-known advertised mode list `(w, h, vrefresh, preferred)`,
    /// preferred-first. Retained across disconnect so a momentarily-gone
    /// monitor keeps reporting its modes until reconnect refreshes them.
    pub modes: Vec<(u16, u16, u32, bool)>,
}
```

Add a `connectors: HashMap<String, ConnectorEntry>` field to `RandrIdAllocator` (rename the struct doc comment to "connector registry"). Keep `next` and `modes` (the mode-id map) fields.

- [ ] **Step 2: Add registry accessor/mutator methods**

```rust
impl RandrIdAllocator {
    // ... existing fresh / ids_for / mode_id / known_connectors ...

    /// Get or create the registry entry for a connector, allocating
    /// stable ids on first sight. New entries default to disconnected,
    /// Off, not-client-configured, empty mode list.
    pub(crate) fn entry_mut(&mut self, name: &str) -> &mut ConnectorEntry {
        if !self.connectors.contains_key(name) {
            let ids = self.ids_for(name);
            self.connectors.insert(
                name.to_string(),
                ConnectorEntry {
                    ids,
                    connected: false,
                    config: ConnectorConfig::Off,
                    client_configured: false,
                    modes: Vec::new(),
                },
            );
        }
        self.connectors.get_mut(name).expect("just inserted")
    }

    pub(crate) fn entry(&self, name: &str) -> Option<&ConnectorEntry> {
        self.connectors.get(name)
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = (&String, &ConnectorEntry)> {
        self.connectors.iter()
    }
}
```

- [ ] **Step 3: Build to verify it compiles**

Run: `cargo build -p yserver --locked 2>&1 | tail -10`
Expected: compiles. The registry is not yet wired into `randr_outputs`/rescan — that happens in later tasks as the enable/disable paths land. This task just introduces the storage.

- [ ] **Step 4: Commit**

```bash
git add crates/yserver/src/kms/v2/backend.rs
git commit -m "feat(randr): grow RandrIdAllocator into a connector registry

Adds ConnectorEntry (connection state, current config, client_configured,
mode list) keyed by connector name. Authoritative store for client-driven
layout preservation. Not yet wired into rescan; storage only.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

### Task 1.5: 2-D `recompute_fb_extent_from` (backend twin of Task 1.2)

**Why here (not Phase 5):** Phase 4's `apply_crtc_config` enable path (Task 4.1) sizes the framebuffer via `recompute_fb_extent_from`. If that still ignores `y`, the *first* vertically-stacked layout a client enables under-sizes the framebuffer and breaks the exact stacked-monitor case this feature unlocks. So the fix must land before Phase 4 — do it in Phase 1 next to the core 2-D fix.

**Files:**
- Modify: `crates/yserver/src/kms/v2/platform.rs:623` (`recompute_fb_extent_from`) + all call sites that build the layout tuples

- [ ] **Step 1: Extend the tuple to carry `y` and compute `fb_h = max(y+h)`**

In `crates/yserver/src/kms/v2/platform.rs`, change `recompute_fb_extent_from` from `(x, w, h)` to `(x, y, w, h)`:

```rust
/// Pure recompute of the virtual-screen extent from `(x, y, width, height)`.
pub(crate) fn recompute_fb_extent_from(layouts: &[(i32, i32, u16, u16)]) -> (u16, u16) {
    let fb_w = layouts
        .iter()
        .map(|(x, _, w, _)| x.saturating_add(i32::from(*w)))
        .map(|v| u16::try_from(v.max(0)).unwrap_or(u16::MAX))
        .max()
        .unwrap_or(0);
    let fb_h = layouts
        .iter()
        .map(|(_, y, _, h)| y.saturating_add(i32::from(*h)))
        .map(|v| u16::try_from(v.max(0)).unwrap_or(u16::MAX))
        .max()
        .unwrap_or(0);
    (fb_w, fb_h)
}
```

- [ ] **Step 2: Update every call site to pass `(x, y, w, h)`**

Find them:

Run: `rg -n "recompute_fb_extent_from" crates/yserver/src/kms/v2/`

For each, change the tuple builder from `(layout.x, layout.width, layout.height)` to `(layout.x, layout.y, layout.width, layout.height)` (e.g. the tail of `requery_outputs_and_modeset` ~:2351). All current callers use `y=0`, so behavior is unchanged today; the field is now honored once clients set non-zero `y` in Phase 4.

- [ ] **Step 3: Build**

Run: `cargo build -p yserver --locked 2>&1 | tail -10`
Expected: compiles (all callers updated).

- [ ] **Step 4: Commit**

```bash
git add crates/yserver/src/kms/v2/platform.rs
git commit -m "fix(kms): 2-D framebuffer extent (max(y+height)) for stacked layouts

Backend twin of the core 2-D bbox fix. Needed before apply_crtc_config so a
client vertical stack doesn't under-size the framebuffer. No behavior change
at y=0.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Phase 2 — Query handlers: full modes, off/disconnected, GetScreenResources reprobe, GetMonitors

Goal: `GetScreenResources`/`GetScreenResourcesCurrent`/`GetOutputInfo`/`GetCrtcInfo` report full mode lists and correct off/disconnected state; `GetMonitors`/XINERAMA report only enabled outputs.

### Task 2.1: Full mode-id list + `nPreferred` in `GetOutputInfo`; deduped union in `GetScreenResources`

**Files:**
- Modify: `crates/yserver-core/src/randr.rs` — `output_info` :221, `OutputInfoReplyData` :276, `screen_resources_current` :182, `from_outputs` modes union
- Modify: `crates/yserver-core/src/core_loop/process_request.rs:2178` (GetOutputInfo handler — pass full mode list)
- Test: `crates/yserver-core/src/randr.rs`

- [ ] **Step 1: Write the failing test (mode union + preferred-first output info)**

Add to `randr.rs` tests:

```rust
    #[test]
    fn output_info_reports_full_mode_list_preferred_first() {
        let outs = vec![RandrOutput {
            name: "HDMI-A-1".into(),
            output_id: 1,
            crtc_id: 2,
            mode_id: 7, // current = 1080p
            connected: true,
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            vrefresh: 60,
            mm_width: 0,
            mm_height: 0,
            mode_ids: vec![7, 8, 9], // 1080p (preferred), 720p, 1360x768
            num_preferred: 1,
        }];
        let st = RandrState::from_outputs(0, outs);
        let info = st.output_info(1, 0).expect("output 1");
        assert_eq!(info.mode_ids, vec![7, 8, 9], "full list, preferred-first");
        assert_eq!(info.num_preferred, 1);
        assert_eq!(info.mode_id, 7, "current mode unchanged");
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p yserver-core output_info_reports_full_mode_list -- --nocapture`
Expected: FAIL to compile — `OutputInfoReplyData` has no `mode_ids`/`num_preferred`.

- [ ] **Step 3: Extend `OutputInfoReplyData` and `output_info`**

In `randr.rs`, add to `OutputInfoReplyData` (`:276`):

```rust
    pub connection: u8,
    /// Full advertised mode-id list, preferred-first.
    pub mode_ids: Vec<u32>,
    /// Leading preferred count (Xorg nPreferred).
    pub num_preferred: u16,
```

In `output_info` (`:247`), populate them from the output:

```rust
        Some(OutputInfoReplyData {
            timestamp: self.timestamp,
            crtc: if out.connected { out.crtc_id } else { 0 },
            mode_id: out.mode_id,
            width_mm,
            height_mm,
            name: out.name.clone(),
            connection: if out.connected { 0 } else { 1 },
            mode_ids: out.mode_ids.clone(),
            num_preferred: out.num_preferred,
        })
```

- [ ] **Step 4: Run the unit test to verify it passes**

Run: `cargo test -p yserver-core output_info_reports_full_mode_list`
Expected: PASS.

- [ ] **Step 5: Wire the full list into the `GetOutputInfo` handler**

In `process_request.rs:2178` (`RR_GET_OUTPUT_INFO`), the handler currently builds `let mode_ids = [info_data.mode_id];`. Replace with the full list and pass `num_preferred`:

```rust
                    let crtc_ids = [info_data.crtc];
                    let mode_ids: &[u32] = &info_data.mode_ids;
                    let name_bytes = info_data.name.as_bytes();
                    let buf = x11randr::encode_get_output_info_reply(
                        byte_order,
                        sequence,
                        &x11randr::OutputInfoReply {
                            timestamp: info_data.timestamp,
                            crtc: info_data.crtc,
                            width_mm: info_data.width_mm,
                            height_mm: info_data.height_mm,
                            connection: info_data.connection,
                            subpixel_order: 0,
                            crtcs: &crtc_ids,
                            modes: mode_ids,
                            clones: &[],
                            name: name_bytes,
                        },
                    );
```

Check `OutputInfoReply` (in `crates/yserver-protocol/src/x11/randr.rs`) carries an `num_preferred`/`n_preferred` field; if `encode_get_output_info_reply` takes it separately, pass `info_data.num_preferred`. If the wire encoder hardcodes `nPreferred = modes.len()`, change it to accept `num_preferred` — verify with:

Run: `rg -n "n_preferred|nPreferred|num_preferred|fn encode_get_output_info_reply|struct OutputInfoReply" crates/yserver-protocol/src/x11/randr.rs`

If the encoder needs the field added, add `pub num_preferred: u16` to `OutputInfoReply` and write it into the reply at the `nPreferred` offset (RANDR `GetOutputInfo` reply: `nPreferred` is a `CARD16` after `nModes`). Add a protocol-crate test asserting the byte at that offset matches.

- [ ] **Step 6: Build the deduped union of ALL modes for `GetScreenResources`**

`screen_resources_current` (`:182`) iterates `self.modes`. Today `self.modes` only holds modes referenced by some output's *current* `mode_id`. For the full advertised union, `from_outputs` must collect geometry for **every** mode id in every output's `mode_ids` — but core only knows geometry for the current mode. Resolve this by having the backend supply a geometry map.

Add to `RandrState`:

```rust
    /// Geometry for every advertised mode id, deduped. Superset of the
    /// per-output `mode_ids`. Populated by the backend bridge; the
    /// `modes` reply vector is built from this.
    pub mode_table: Vec<RandrMode>,
```

Have `randr_outputs()`'s caller (the KMS bridge, Task 2.2) also produce a `Vec<RandrMode>` and pass it to a new `from_outputs_with_modes(timestamp, outputs, mode_table)`. Keep `from_outputs` as a thin wrapper that derives `mode_table` from the current modes (so ynest/tests stay correct). Update `screen_resources_current` to emit `self.mode_table` instead of `self.modes` for the `modes`/`mode_names` vectors. Keep `self.modes` (current-mode dedup) for back-compat with notify code that reads it.

```rust
    #[must_use]
    pub fn from_outputs(timestamp: u32, outputs: Vec<RandrOutput>) -> Self {
        let mode_table = Self::current_mode_table(&outputs);
        Self::from_outputs_with_modes(timestamp, outputs, mode_table)
    }
```

Write `current_mode_table` to reproduce today's dedup-by-current-mode loop, and move the body of the old `from_outputs` into `from_outputs_with_modes(timestamp, outputs, mode_table)` storing `mode_table`.

- [ ] **Step 7: Add a union-dedup test**

```rust
    #[test]
    fn screen_resources_emits_full_deduped_mode_union() {
        let modes = vec![
            RandrMode { mode_id: 7, width: 1920, height: 1080, vrefresh: 60 },
            RandrMode { mode_id: 8, width: 1280, height: 720, vrefresh: 60 },
        ];
        let out = RandrOutput {
            name: "HDMI-A-1".into(), output_id: 1, crtc_id: 2, mode_id: 7,
            connected: true, x: 0, y: 0, width: 1920, height: 1080, vrefresh: 60,
            mm_width: 0, mm_height: 0, mode_ids: vec![7, 8], num_preferred: 1,
        };
        let st = RandrState::from_outputs_with_modes(0, vec![out], modes);
        let res = st.screen_resources_current();
        assert_eq!(res.modes.len(), 2, "both advertised modes in resources");
        assert!(res.modes.iter().any(|m| m.id == 8 && m.width == 1280));
    }
```

Run: `cargo test -p yserver-core screen_resources_emits_full_deduped_mode_union`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/yserver-core/src/randr.rs crates/yserver-core/src/core_loop/process_request.rs crates/yserver-protocol/src/x11/randr.rs
git commit -m "feat(randr): GetOutputInfo full mode list + nPreferred; GetScreenResources mode union

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

### Task 2.2: KMS bridge supplies the full mode table; `GetScreenResources` force-reprobe

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs:2416` (`randr_outputs` → also return mode table), `fire_randr_changes` :4889
- Modify: `crates/yserver-core/src/backend/trait_def.rs` (add `reprobe_connectors`), `recording.rs`, host_x11 impl
- Modify: `crates/yserver-core/src/core_loop/process_request.rs:2167` (split GetScreenResources vs Current)

- [ ] **Step 1: Add ONE consolidated `rebuild_randr_state` helper (unify rebuild paths)**

> **Risk fix (codex):** the plan must not grow two divergent paths that rebuild `state.randr` (`fire_randr_changes` for hotplug, a separate `refresh_randr_state` for SetCrtcConfig, plus reprobe). Divergent timestamp/notify handling between hotplug and `SetCrtcConfig` is a real bug class. Consolidate into one private method on `KmsBackendV2` that every path calls.

In `backend.rs`, add a sibling to `randr_outputs` that returns both outputs and the deduped mode table, then the single rebuild helper:

```rust
    /// The one place `state.randr` is rebuilt from the backend's
    /// registry/outputs.
    /// - `set_time`: `Some(t)` sets `timestamp` (lastSetTime) to `t`
    ///   (SetCrtcConfig uses the client-provided request timestamp;
    ///   hotplug uses `timestamp_now()`); `None` preserves the prior
    ///   value (a no-op re-probe must not advance lastSetTime).
    /// - `config_changed`: advances `config_timestamp` (lastConfigTime)
    ///   only when the available config (outputs/modes/connection)
    ///   changed — i.e. hotplug/reprobe-with-change, NOT a CRTC set.
    ///
    /// The current logical screen size (`screen_width`/`screen_height`
    /// + `*_mm`) is OWNED by `RRSetScreenSize` once set and is carried
    /// forward across every rebuild — a re-probe or CRTC set never
    /// collapses a client-resized screen back to the bounding box
    /// (Xorg keeps `pScreen->width/height` until the client resizes).
    fn rebuild_randr_state(
        &mut self,
        state: &mut ServerState,
        set_time: Option<u32>,
        config_changed: bool,
    ) {
        let prev_ts = state.randr.timestamp;
        let prev_ct = state.randr.config_timestamp;
        // Snapshot the client-owned logical screen size to carry forward.
        let prev_screen = (
            state.randr.screen_width,
            state.randr.screen_height,
            state.randr.width_mm,
            state.randr.height_mm,
        );
        let (outputs, mode_table) = self.randr_outputs_and_modes();
        let new_ts = set_time.unwrap_or(prev_ts);
        let ts_now = state.timestamp_now();
        state.randr =
            yserver_core::randr::RandrState::from_outputs_with_modes(new_ts, outputs, mode_table);
        // Carry forward the client-set logical size (from_outputs reseeds
        // it to the bbox; that is only correct at boot, where prev_screen
        // already equals the bbox).
        state.randr.screen_width = prev_screen.0;
        state.randr.screen_height = prev_screen.1;
        state.randr.width_mm = prev_screen.2;
        state.randr.height_mm = prev_screen.3;
        state.randr.config_timestamp = if config_changed { ts_now } else { prev_ct };
    }
```

Change `fire_randr_changes` (`:4911`) to call `self.rebuild_randr_state(state, Some(state.timestamp_now()), true)` instead of inlining `from_outputs`. `SetCrtcConfig` (Task 4.2) and the screen-size path also go through this helper — no second rebuild path.

> **Boot caveat:** the very first `RandrState` is built directly by `from_outputs`/`with_randr_outputs` (boot/nested), which correctly seeds `screen_*` from the bounding box. `rebuild_randr_state` only ever runs *after* that, so `prev_screen` is always a meaningful prior size — boot extend-right is preserved, and a client resize is preserved thereafter.

Implement `randr_outputs_and_modes` by refactoring `randr_outputs` to also accumulate a `HashMap<(u16,u16,u32), RandrMode>` while it walks each connector's modes, then collect+sort by `mode_id`.

- [ ] **Step 2: Add `reprobe_connectors` to the `Backend` trait (error-propagating)**

> **Fix (codex):** the re-probe must surface failure to the caller (Xorg `RRGetInfo(force_query=TRUE)` returns `BadAlloc` at the request level on failure) and must NOT bump timestamps when nothing changed. Return a `Result` and let the handler map `Err` → `BadAlloc`.

In `crates/yserver-core/src/backend/trait_def.rs`, near the other loop hooks (after `on_display_hotplug` ~:316), add:

```rust
    /// Force a connector re-probe (RANDR `GetScreenResources`,
    /// `force_query=TRUE` in Xorg `RRGetInfo`). Re-reads connection
    /// state + mode lists into the registry WITHOUT changing any
    /// enabled output's config, and rebuilds `state.randr`. Bumps
    /// `config_timestamp` ONLY when connection state / mode lists
    /// changed; leaves both timestamps untouched on a no-op probe.
    /// `Err` is surfaced by the handler as `BadAlloc`. Default
    /// `Ok(())` no-op for fixed-topology backends (ynest, recording).
    fn reprobe_connectors(&mut self, _state: &mut ServerState) -> io::Result<()> {
        Ok(())
    }
```

Default `Ok(())` means ynest/host-x11 and recording need no change. Confirm:

Run: `cargo build -p yserver-core --locked 2>&1 | tail -5`
Expected: compiles (default body).

- [ ] **Step 3: Implement `reprobe_connectors` on the KMS v2 backend**

In `crates/yserver/src/kms/v2/backend.rs` `impl Backend for KmsBackendV2`, add:

```rust
    fn reprobe_connectors(&mut self, state: &mut ServerState) -> io::Result<()> {
        // Reconcile the registry with the hardware without disturbing
        // any enabled output (no auto-enable, no recompact). Mirrors
        // requery_outputs_and_modeset's discover/diff, minus modeset.
        let changed = self.platform.reprobe_connectors()?;
        // Pure re-probe: never bumps lastSetTime (set_time = None); bumps
        // lastConfigTime only when something actually changed. No-op probe
        // leaves both timestamps + the client-set screen size intact.
        self.rebuild_randr_state(state, None, changed);
        Ok(())
    }
```

Add `PlatformBackend::reprobe_connectors(&mut self) -> io::Result<bool>` in `platform.rs`: run `discover_outputs` (propagate its `io::Error` — a failed probe means card-gone/alloc-fail and maps to `BadAlloc`), update each registry entry's `connected` flag + `modes`, return `true` if any connection state or mode list changed. It must NOT touch `self.outputs` (enabled set) or call `recompact_horizontal_layout`.

- [ ] **Step 4: Split `GetScreenResources` (reprobe) from `GetScreenResourcesCurrent` (cached)**

In `process_request.rs:2167`, the two are currently merged. Split them so `RR_GET_SCREEN_RESOURCES` calls `backend.reprobe_connectors(state)` first. This handler must have a `backend: &mut dyn Backend` in scope — check the `handle_randr_request` signature; if it lacks `backend`, thread it through (the dispatch at `process_request.rs:384` passes `state, client_id, sequence, header, body` — find the variant that also has backend, e.g. how `handle_warp_pointer` gets `backend`). Then:

```rust
                x11randr::RR_GET_SCREEN_RESOURCES => {
                    if let Err(e) = backend.reprobe_connectors(state) {
                        log::warn!("RRGetScreenResources reprobe failed: {e}");
                        return emit_x11_error_with_minor(
                            state, client_id, sequence, x11::error::BAD_ALLOC,
                            0, u16::from(header.data), RANDR_MAJOR_OPCODE,
                        );
                    }
                    let resources = state.randr.screen_resources_current();
                    // ... encode + write (same as before) ...
                }
                x11randr::RR_GET_SCREEN_RESOURCES_CURRENT => {
                    let resources = state.randr.screen_resources_current();
                    // ... encode + write ...
                }
```

Confirm `x11::error::BAD_ALLOC` exists (`rg -n "BAD_ALLOC" crates/yserver-protocol/src/x11`).

If threading `backend` into `handle_randr_request` is invasive, add a narrower path: have the dispatch call a small `handle_randr_get_screen_resources(state, backend, ...)` for opcode `RR_GET_SCREEN_RESOURCES` only, keeping the rest in the existing `handle_randr_request`.

- [ ] **Step 5: Build + smoke the query path under ynest**

Run: `cargo build --locked 2>&1 | tail -5 && cargo test -p yserver-core randr`
Expected: clean. ynest's `reprobe_connectors` is the default no-op, so `GetScreenResources` behaves as before there.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(randr): GetScreenResources force-reprobe via backend hook; cached Current

Adds Backend::reprobe_connectors (no-op default; KMS reconciles registry).
Bridge supplies full deduped mode table. config_timestamp bumps on change.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

### Task 2.3: `GetMonitors`/XINERAMA report enabled outputs only

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs:2087` (`active_monitors`)
- Test: `crates/yserver-core/src/core_loop/process_request.rs` or a randr-level test exercising `active_monitors`

- [ ] **Step 1: Write the failing test**

`active_monitors` is `fn active_monitors(state: &ServerState)`. Add a test in `process_request.rs`'s test module (or create one) that builds a `ServerState` with a mix of enabled/off/disconnected outputs and asserts only enabled appear. If `ServerState` is heavy to construct in a unit test, instead add a pure helper `enabled_monitor_rows(&RandrState) -> Vec<ActiveMonitor>` in `randr.rs` and test that. Prefer the pure helper:

In `randr.rs`, add:

```rust
    /// Monitors for RANDR GetMonitors / XINERAMA: one per ENABLED
    /// output (connected with a non-zero mode), at its `(x,y,w,h)`.
    /// Off and disconnected outputs are absent (Xorg builds an
    /// automatic monitor only for an output with an active CRTC).
    #[must_use]
    pub fn enabled_outputs(&self) -> impl Iterator<Item = &RandrOutput> {
        self.outputs
            .iter()
            .filter(|o| o.connected && o.mode_id != 0)
    }
```

Test:

```rust
    #[test]
    fn enabled_outputs_excludes_off_and_disconnected() {
        let outs = vec![
            RandrOutput { // enabled
                name: "eDP-1".into(), output_id: 1, crtc_id: 2, mode_id: 3,
                connected: true, x: 0, y: 0, width: 1920, height: 1080, vrefresh: 60,
                mm_width: 0, mm_height: 0, mode_ids: vec![3], num_preferred: 1,
            },
            RandrOutput { // connected but OFF (mode_id 0)
                name: "HDMI-A-1".into(), output_id: 4, crtc_id: 5, mode_id: 0,
                connected: true, x: 0, y: 0, width: 0, height: 0, vrefresh: 0,
                mm_width: 0, mm_height: 0, mode_ids: vec![6], num_preferred: 1,
            },
            RandrOutput { // disconnected
                name: "DP-2".into(), output_id: 7, crtc_id: 8, mode_id: 0,
                connected: false, x: 0, y: 0, width: 0, height: 0, vrefresh: 0,
                mm_width: 0, mm_height: 0, mode_ids: vec![], num_preferred: 0,
            },
        ];
        let st = RandrState::from_outputs(0, outs);
        let names: Vec<&str> = st.enabled_outputs().map(|o| o.name.as_str()).collect();
        assert_eq!(names, vec!["eDP-1"]);
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p yserver-core enabled_outputs_excludes -- --nocapture`
Expected: FAIL to compile (`enabled_outputs` not defined).

- [ ] **Step 3: Implement and rewire `active_monitors`**

Add `enabled_outputs` (above). In `process_request.rs:2087`, change `active_monitors` to iterate `state.randr.enabled_outputs()` instead of `state.randr.outputs.iter().enumerate()`, computing `primary` as "is this the `primary_output`?" rather than `i == 0` (since the first output may now be off/disconnected). Keep the `width_mm`/`height_mm` fallback logic.

```rust
fn active_monitors(state: &ServerState) -> Vec<ActiveMonitor> {
    let primary = state.randr.primary_output;
    state
        .randr
        .enabled_outputs()
        .map(|output| {
            let width_mm = if output.mm_width > 0 {
                output.mm_width
            } else {
                ((u32::from(output.width) * 254 + 480) / 960).max(1)
            };
            let height_mm = if output.mm_height > 0 {
                output.mm_height
            } else {
                ((u32::from(output.height) * 254 + 480) / 960).max(1)
            };
            ActiveMonitor {
                name: output.name.clone(),
                output_id: output.output_id,
                primary: output.output_id == primary,
                x: output.x,
                y: output.y,
                width: output.width,
                height: output.height,
                width_mm,
                height_mm,
            }
        })
        .collect()
}
```

Ensure `primary_output` prefers an *enabled* output. `from_outputs` currently picks the first connected; tighten it to prefer `connected && mode_id != 0`, falling back to first connected, then first. Update the `find` in `from_outputs:107`:

```rust
        let primary_output = outputs
            .iter()
            .find(|o| o.connected && o.mode_id != 0)
            .or_else(|| outputs.iter().find(|o| o.connected))
            .or_else(|| outputs.first())
            .map_or(0, |o| o.output_id);
```

- [ ] **Step 4: Run the test + existing monitor/xinerama tests**

Run: `cargo test -p yserver-core enabled_outputs_excludes && cargo test -p yserver-core`
Expected: PASS. Watch for any existing test asserting `primary = i==0`; update it to the `primary_output` semantics if needed.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver-core/src/randr.rs crates/yserver-core/src/core_loop/process_request.rs
git commit -m "feat(randr): GetMonitors/XINERAMA report enabled outputs only

One automatic monitor per enabled output at its (x,y,w,h); off and
disconnected outputs absent. primary prefers an enabled output.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Phase 3 — `RRSetScreenSize` + `set_logical_screen_size`

Goal: a client can resize the logical screen; validation rejects a crop; the resize re-clamps/warps the pointer and fans out notifications.

### Task 3.0: Protocol-crate support for `RRSetScreenSize` (prerequisite)

> **Fix (codex):** the protocol crate (`crates/yserver-protocol/src/x11/randr.rs:38-56`) defines only `RR_SET_SCREEN_CONFIG` (2) and `RR_SET_CRTC_CONFIG` (21) — there is **no** `RR_SET_SCREEN_SIZE` (RANDR 1.2 opcode **7**) constant or body parser. Phase 3 cannot dispatch the request until this lands. Do it first.

**Files:**
- Modify: `crates/yserver-protocol/src/x11/randr.rs` (opcode const + parser + round-trip test)

- [ ] **Step 1: Add the opcode constant**

In `crates/yserver-protocol/src/x11/randr.rs`, with the other `RR_*` consts (after `RR_GET_SCREEN_SIZE_RANGE: u8 = 6;`):

```rust
pub const RR_SET_SCREEN_SIZE: u8 = 7;
```

- [ ] **Step 2: Write the failing parser round-trip test**

RANDR `SetScreenSize` body (post-X11-request-header): `window(4) width(CARD16) height(CARD16) widthInMillimeters(CARD32) heightInMillimeters(CARD32)`.

```rust
    #[test]
    fn parse_set_screen_size_roundtrip() {
        // window=0x200, 2560x1440, 677x381 mm — little-endian.
        let body = [
            0x00, 0x02, 0x00, 0x00, // window
            0x00, 0x0a, // width = 2560
            0xa0, 0x05, // height = 1440
            0xa5, 0x02, 0x00, 0x00, // mm_width = 677
            0x7d, 0x01, 0x00, 0x00, // mm_height = 381
        ];
        let r = parse_set_screen_size_request(&body).expect("valid");
        assert_eq!(r.width, 2560);
        assert_eq!(r.height, 1440);
        assert_eq!(r.mm_width, 677);
        assert_eq!(r.mm_height, 381);
    }
```

Run: `cargo test -p yserver-protocol parse_set_screen_size_roundtrip -- --nocapture`
Expected: FAIL to compile (`parse_set_screen_size_request` missing).

- [ ] **Step 3: Implement the parser**

```rust
pub struct SetScreenSizeRequest {
    pub window: u32,
    pub width: u16,
    pub height: u16,
    pub mm_width: u32,
    pub mm_height: u32,
}

#[must_use]
pub fn parse_set_screen_size_request(body: &[u8]) -> Option<SetScreenSizeRequest> {
    if body.len() < 16 {
        return None;
    }
    Some(SetScreenSizeRequest {
        window: u32::from_le_bytes(body[0..4].try_into().ok()?),
        width: u16::from_le_bytes(body[4..6].try_into().ok()?),
        height: u16::from_le_bytes(body[6..8].try_into().ok()?),
        mm_width: u32::from_le_bytes(body[8..12].try_into().ok()?),
        mm_height: u32::from_le_bytes(body[12..16].try_into().ok()?),
    })
}
```

(Match the byte-order convention used by the crate's other RANDR parsers — if they read with a passed `byte_order` rather than hardcoded LE, follow that; check `parse_crtc_request`/`parse_output_request` in the same file first.)

- [ ] **Step 4: Run the test**

Run: `cargo test -p yserver-protocol parse_set_screen_size_roundtrip`
Expected: PASS.

- [ ] **Step 5: Check exact-length validation does not reject SetScreenSize**

`process_request.rs` runs `validate_exact_request_length` before dispatch (`:120`). Add `RR_SET_SCREEN_SIZE` to the request-length table if RANDR sub-opcodes are validated there; otherwise confirm RANDR requests bypass it. Grep:

Run: `rg -n "RR_SET_CRTC_CONFIG|randr|exact_required_length" crates/yserver-protocol/src/x11/request_lengths.rs | head`

- [ ] **Step 6: Commit**

```bash
git add crates/yserver-protocol/src/x11/randr.rs crates/yserver-protocol/src/x11/request_lengths.rs
git commit -m "feat(protocol): RR_SET_SCREEN_SIZE opcode + body parser

RANDR 1.2 SetScreenSize (opcode 7) was missing from the protocol crate.
Prerequisite for the RRSetScreenSize handler.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

### Task 3.1: `set_logical_screen_size` backend method

**Files:**
- Modify: `crates/yserver-core/src/backend/trait_def.rs` (add method), `recording.rs`
- Modify: `crates/yserver/src/kms/v2/backend.rs` (impl), `crates/yserver/src/kms/v2/platform.rs` (root/COW backing resize)

- [ ] **Step 1: Add the trait method (default `Ok(())` no-op)**

In `trait_def.rs`, after `reprobe_connectors`:

```rust
    /// Resize the logical (virtual) screen to `w`×`h`: reallocate the
    /// root + Composite-overlay backing storage and update the pointer
    /// clamp / logical extent. Default no-op `Ok(())` for nested
    /// backends (ynest follows its host window size instead).
    fn set_logical_screen_size(&mut self, _w: u16, _h: u16) -> io::Result<()> {
        Ok(())
    }
```

- [ ] **Step 2: Implement on KMS v2**

In `backend.rs` `impl Backend for KmsBackendV2`:

```rust
    fn set_logical_screen_size(&mut self, w: u16, h: u16) -> io::Result<()> {
        self.platform.set_logical_screen_size(w, h)?;
        // Logical extent changed → scene must re-derive output offsets
        // and root/COW sampling. Quiesce, rebuild.
        self.platform.wait_idle_bounded();
        self.scene.drain_all(&mut self.platform);
        self.scene
            .rebuild_outputs(&self.platform)
            .map_err(|e| io::Error::other(format!("scene rebuild after screen resize: {e:?}")))?;
        Ok(())
    }
```

Add `PlatformBackend::set_logical_screen_size(&mut self, w: u16, h: u16) -> io::Result<()>` in `platform.rs`: reallocate the root + COW backing (find where root/COW storage is first allocated at `fb_w×fb_h` and factor a resize path), set `self.fb_w = w; self.fb_h = h;`. Per the open question in the spec, resize whichever backs the logical screen (root when comp off, COW when comp on); confirm both during impl.

Run: `rg -n "fb_w|fb_h|cow_id|allocate_drawable_storage|root.*storage" crates/yserver/src/kms/v2/backend.rs crates/yserver/src/kms/v2/platform.rs | head -30`
to locate the root/COW allocation sites before writing the resize.

- [ ] **Step 3: Build**

Run: `cargo build --locked 2>&1 | tail -10`
Expected: compiles. (Behavioral verification is HW-gated — §HW acceptance.)

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "feat(randr): set_logical_screen_size backend method (root/COW resize)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

### Task 3.2: `RRSetScreenSize` handler with crop validation, pointer warp, fanout

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` (add a real `RR_SET_SCREEN_SIZE` arm — currently RANDR 1.0 `RR_SET_SCREEN_CONFIG` is the no-op at :2454; `RR_SET_SCREEN_SIZE` is the RANDR 1.2 request MATE uses)
- Modify: `crates/yserver-core/src/randr.rs` (a `set_logical_size` mutator + crop check helper)
- Modify: `crates/yserver-core/src/core_loop/run.rs` (reuse `handle_host_container_resize` machinery + add pointer warp)
- Test: `crates/yserver-core/src/randr.rs` (crop-reject + range validation), and a handler-level test for the warp

- [ ] **Step 1: Confirm the opcode + parser exist**

Run: `rg -n "RR_SET_SCREEN_SIZE|parse_set_screen_size|SetScreenSize" crates/yserver-protocol/src/x11/randr.rs crates/yserver-core/src/core_loop/process_request.rs`
Expected: find `RR_SET_SCREEN_SIZE` const (opcode 7). If no body parser exists, note that the body layout is: `window(4) width(2) height(2) widthInMillimeters(4) heightInMillimeters(4)`. Add `parse_set_screen_size_request(body) -> Option<{width, height, mm_width, mm_height}>` to the protocol crate with a round-trip test.

- [ ] **Step 2: Write the failing validation tests**

In `randr.rs` tests:

```rust
    #[test]
    fn set_screen_size_rejects_crop_of_enabled_output() {
        // eDP at (0,0) 1920x1080 enabled. Shrinking the screen to
        // 1280x720 would crop it → BadMatch (caller maps Err to the
        // protocol error).
        let outs = vec![RandrOutput {
            name: "eDP-1".into(), output_id: 1, crtc_id: 2, mode_id: 3,
            connected: true, x: 0, y: 0, width: 1920, height: 1080, vrefresh: 60,
            mm_width: 0, mm_height: 0, mode_ids: vec![3], num_preferred: 1,
        }];
        let st = RandrState::from_outputs(0, outs);
        assert!(st.screen_size_would_crop(1280, 720), "1280x720 crops 1920x1080");
        assert!(!st.screen_size_would_crop(2560, 1440), "larger does not crop");
    }
```

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test -p yserver-core set_screen_size_rejects_crop -- --nocapture`
Expected: FAIL to compile (`screen_size_would_crop` missing).

- [ ] **Step 4: Implement the crop check + logical-size mutator**

In `randr.rs`:

```rust
    /// Would shrinking the logical screen to `w`×`h` crop any enabled
    /// output? (Xorg `RRSetScreenSize` BadMatch, rrscreen.c:266.)
    #[must_use]
    pub fn screen_size_would_crop(&self, w: u16, h: u16) -> bool {
        self.outputs.iter().filter(|o| o.connected && o.mode_id != 0).any(|o| {
            i32::from(o.x) + i32::from(o.width) > i32::from(w)
                || i32::from(o.y) + i32::from(o.height) > i32::from(h)
        })
    }

    /// Set the logical (reported) screen size after validation. Uses
    /// the CLIENT-supplied physical mm verbatim (Xorg `RRScreenSizeSet`
    /// passes `stuff->widthInMillimeters`/`heightInMillimeters` — it does
    /// NOT recompute from pixels). Does not touch outputs.
    pub fn set_logical_size(&mut self, timestamp: u32, w: u16, h: u16, mm_w: u32, mm_h: u32) {
        self.screen_width = w;
        self.screen_height = h;
        self.width_mm = mm_w;
        self.height_mm = mm_h;
        self.timestamp = timestamp.max(1);
        self.config_timestamp = self.timestamp;
    }
```

- [ ] **Step 5: Run the validation test**

Run: `cargo test -p yserver-core set_screen_size_rejects_crop`
Expected: PASS.

- [ ] **Step 6: Add the `RR_SET_SCREEN_SIZE` handler**

In `process_request.rs` `handle_randr_request` (it needs `backend` in scope — thread it as in Task 2.2 Step 4), add an arm. Validation order per spec §Error handling:

```rust
                x11randr::RR_SET_SCREEN_SIZE => {
                    let Some(req) = x11randr::parse_set_screen_size_request(body) else {
                        return emit_x11_error_with_minor(
                            state, client_id, sequence, x11::error::BAD_VALUE,
                            0, u16::from(header.data), RANDR_MAJOR_OPCODE,
                        );
                    };
                    // Validation order matches rrscreen.c: width range →
                    // height range → crop → zero-mm. errorValue is the
                    // offending dimension (width vs height reported
                    // separately), not always width.
                    let (min_w, min_h, max_w, max_h) = state.randr.screen_size_range();
                    if req.width < min_w || req.width > max_w {
                        return emit_x11_error_with_minor(
                            state, client_id, sequence, x11::error::BAD_VALUE,
                            u32::from(req.width), u16::from(header.data), RANDR_MAJOR_OPCODE,
                        );
                    }
                    if req.height < min_h || req.height > max_h {
                        return emit_x11_error_with_minor(
                            state, client_id, sequence, x11::error::BAD_VALUE,
                            u32::from(req.height), u16::from(header.data), RANDR_MAJOR_OPCODE,
                        );
                    }
                    if state.randr.screen_size_would_crop(req.width, req.height) {
                        return emit_x11_error_with_minor(
                            state, client_id, sequence, x11::error::BAD_MATCH,
                            0, u16::from(header.data), RANDR_MAJOR_OPCODE,
                        );
                    }
                    if req.mm_width == 0 || req.mm_height == 0 {
                        return emit_x11_error_with_minor(
                            state, client_id, sequence, x11::error::BAD_VALUE,
                            0, u16::from(header.data), RANDR_MAJOR_OPCODE,
                        );
                    }
                    if let Err(e) = backend.set_logical_screen_size(req.width, req.height) {
                        log::warn!("RRSetScreenSize: backend resize failed: {e}");
                        // Xorg ProcRRSetScreenSize returns BadMatch when
                        // RRScreenSizeSet fails (rrscreen.c). Silently
                        // succeeding would make the client believe the
                        // resize took. Leave prior state intact + report.
                        return emit_x11_error_with_minor(
                            state, client_id, sequence, x11::error::BAD_MATCH,
                            0, u16::from(header.data), RANDR_MAJOR_OPCODE,
                        );
                    }
                    let ts = state.timestamp_now();
                    state.randr.set_logical_size(ts, req.width, req.height, req.mm_width, req.mm_height);
                    // Pure screen-size change: fire root ConfigureNotify +
                    // ScreenChangeNotify ONLY — no per-CRTC/Output change
                    // (CRTC positions are unchanged). Pass an empty changed
                    // list (see Step 7).
                    apply_screen_size_side_effects(state, backend, req.width, req.height, &[]);
                    // RRSetScreenSize has NO reply (it is a void request).
                    return Ok(RequestOutcome::Handled);
                }
```

Verify `RR_SET_SCREEN_SIZE` is reply-less in the protocol (it is in core RANDR — only `SetScreenConfig`/`SetCrtcConfig` reply). If your dispatch expects a reply, return `Handled` with no write.

- [ ] **Step 7: Add `apply_screen_size_side_effects` (root ConfigureNotify + RANDR fanout + pointer warp)**

In `run.rs` (next to `handle_host_container_resize`), add a shared helper and have `handle_host_container_resize` call it too (DRY — both do root size update + ConfigureNotify + RANDR fanout).

> **Risk fix (codex):** a *pure* screen-size change must NOT fan out per-CRTC/Output change notifies — Xorg's `RRScreenSizeSet` fires only `RRScreenChangeNotify` (+ root `ConfigureNotify`); CRTC positions are unchanged so no `CrtcChangeNotify`/`OutputChangeNotify`. The caller passes the `changed` list explicitly: `RRSetScreenSize` passes `&[]` (ScreenChange only); the nested host-resize path passes its genuinely-resized output. `emit_randr_change_notifications` already fires `ScreenChangeNotify` unconditionally and per-`changed`-entry Crtc/Output notifies, so an empty `changed` yields ScreenChange-only — exactly Xorg's shape.

```rust
/// Common side-effects of a logical-screen-size change: update root +
/// overlay window records, emit root ConfigureNotify, fan out RANDR
/// notifies (ScreenChange always; Crtc/Output only for entries in
/// `changed`), and re-clamp/warp the pointer. `changed` is empty for a
/// pure RRSetScreenSize (CRTCs unchanged).
pub(crate) fn apply_screen_size_side_effects(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    width: u16,
    height: u16,
    changed: &[(u32, u32, u32)],
) {
    use yserver_protocol::x11;
    if let Some(root) = state.resources.window_mut(crate::resources::ROOT_WINDOW) {
        root.width = width;
        root.height = height;
    }
    if let Some(overlay) = state
        .resources
        .window_mut(crate::resources::COMPOSITE_OVERLAY_WINDOW)
    {
        overlay.width = width;
        overlay.height = height;
    }
    let _dropped = crate::core_loop::fanout::emit_window_event_to_state(
        state,
        crate::resources::ROOT_WINDOW,
        0x0002_0000, // StructureNotifyMask
        |buf, seq, order| {
            x11::encode_configure_notify_event(
                buf, seq, order,
                crate::resources::ROOT_WINDOW,
                crate::resources::ROOT_WINDOW,
                None,
                x11::Geometry {
                    root: crate::resources::ROOT_WINDOW,
                    x: 0, y: 0, width, height, border_width: 0, depth: 24,
                },
                false,
            );
        },
    );
    emit_randr_change_notifications(state, changed);

    // Pointer: clamp into [0,w)×[0,h); if the screen shrank below the
    // current cursor position, warp it inside (Xorg
    // RRPointerScreenConfigured / ScreenRestructured). The KMS motion
    // clamp only applies on the NEXT motion, so the explicit warp is
    // required to avoid a stranded off-screen cursor.
    let (px, py) = state.pointer_root;
    let cx = i32::from(px).clamp(0, i32::from(width.saturating_sub(1)));
    let cy = i32::from(py).clamp(0, i32::from(height.saturating_sub(1)));
    if cx != i32::from(px) || cy != i32::from(py) {
        backend.warp_pointer_root(state, cx, cy);
    }
}
```

Refactor `handle_host_container_resize` to call this helper for the root/overlay + ConfigureNotify + fanout portion (keep its `state.randr.resize(...)` call before the helper, since the nested path resizes the single output rather than the logical screen). It passes its own `changed` list (the single resized output `(output_id, crtc_id, mode_id)`) — the nested output genuinely changes geometry, so `CrtcChangeNotify` there is correct and preserved.

- [ ] **Step 8: Add a handler-level pointer-warp test**

Add a test (in `process_request.rs` tests, using `RecordingBackend` which now records `warp_pointer_root` calls — add a recording field if absent) that drives a screen shrink with the cursor at (2000, 1500) and asserts a warp to within the new bounds was issued. If `RecordingBackend` doesn't record warps, add a `pub warped_to: Option<(i32,i32)>` field and set it in its `warp_pointer_root` impl.

```rust
    #[test]
    fn screen_shrink_warps_stranded_cursor_inside() {
        // Build minimal state with one enabled 1280x720 output, cursor
        // parked at (2000,1500) (was valid under an earlier larger
        // screen). Shrinking logical size to 1280x720 must warp inside.
        // ... construct ServerState + RecordingBackend ...
        // apply_screen_size_side_effects(&mut state, &mut backend, 1280, 720);
        // assert_eq!(backend.warped_to, Some((1279, 719)));
    }
```

Fill in `ServerState`/`RecordingBackend` construction following an existing handler test in the same file as a template (search for an existing test that builds both).

- [ ] **Step 9: Run all tests + build**

Run: `cargo test -p yserver-core && cargo build --locked 2>&1 | tail -5`
Expected: PASS / clean.

- [ ] **Step 10: Commit**

```bash
git add -A
git commit -m "feat(randr): RRSetScreenSize handler — crop check, pointer warp, fanout

Validates range/crop/zero-mm (BadValue/BadMatch), resizes logical screen
via backend, emits root ConfigureNotify + RANDR notifies, warps a stranded
cursor inside the new bounds. Shares side-effect helper with host resize.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Phase 4 — `RRSetCrtcConfig` + `apply_crtc_config`

Goal: replace the no-op accept with real validation + DRM apply (enable/disable/mode-change), driving the registry.

### Task 4.1: `ModeSpec` type + `apply_crtc_config` backend method

**Files:**
- Modify: `crates/yserver-core/src/backend/trait_def.rs` (`ModeSpec` + method), `recording.rs`
- Modify: `crates/yserver/src/kms/v2/backend.rs` (impl), `platform.rs` (apply/disable + scanout pool)

- [ ] **Step 1: Define `ModeSpec` and the trait method**

In `trait_def.rs`, near the top (with other backend-facing types), add:

```rust
/// A resolved display mode passed from core RANDR to the backend so it
/// can find the exact DRM mode without a core→DRM type leak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeSpec {
    pub width: u16,
    pub height: u16,
    pub vrefresh: u32,
}
```

And the method (default no-op `Ok(())`):

```rust
    /// Apply a client-driven CRTC configuration to `connector`.
    /// `mode = None` disables the output (frees its scanout, removes it
    /// from the active set, registry → Off, connector stays known).
    /// `mode = Some` enables/changes it at `(x, y)` (reallocating the
    /// scanout pool on a resolution change), registry → Enabled. On
    /// DDX/alloc failure returns Err and leaves the server in its prior
    /// consistent state (no partial enable). Default no-op for nested.
    fn apply_crtc_config(
        &mut self,
        _connector: &str,
        _mode: Option<ModeSpec>,
        _x: i32,
        _y: i32,
    ) -> io::Result<()> {
        Ok(())
    }
```

- [ ] **Step 2: Implement `apply_crtc_config` on KMS v2 (disable path)**

In `backend.rs` `impl Backend for KmsBackendV2`. Disable: find the `OutputLayout` for `connector`, `disable_output` it, free its scanout pool, remove from `platform.outputs` + scene, registry → Off, recompute fb extent, rebuild scene. Reuse the existing topology-change path (the resume/rescan code at `backend.rs:4901` does `wait_idle_bounded` → `scene.drain_all` → `scene.rebuild_outputs`). Factor a `PlatformBackend::disable_connector(&mut self, connector: &str) -> io::Result<()>` that does the DRM disable + pool free + `outputs` removal + `recompute_fb_extent_from` (NO recompact), then the backend does the scene quiesce/rebuild.

- [ ] **Step 3: Implement the enable/mode-change path**

`PlatformBackend::enable_connector(&mut self, connector: &str, mode: ModeSpec, x: i32, y: i32) -> io::Result<()>`:
- Resolve `connector` → the discovered `Output` (re-run `discover_outputs` or use a cached discovery) and map `ModeSpec` → the matching `drm::control::Mode` (match `(width, height, vrefresh)` against `output.modes`/the DRM mode list); set `output.mode`/`output.picked`.
- If not currently enabled or resolution changed, (re)allocate the `ScanoutBoPool` at the new size (mirror the alloc in `requery_outputs_and_modeset`'s add path, `platform.rs:2420-2522`).
- `commit_modeset` at the mode.
- Set the `OutputLayout` `{x, y, width, height}`; add to / update `platform.outputs`.
- `recompute_fb_extent_from` (2-D), update `fb_w/fb_h` — but do NOT recompact (client owns position).
- Registry entry → `Enabled { mode_w, mode_h, vrefresh, x, y }`, `client_configured = true`, `connected = true`.

On failure after pool alloc but before commit: free the freshly-allocated pool and leave the output off (spec §Error handling). The backend wraps this with the scene quiesce/rebuild.

- [ ] **Step 4: Build**

Run: `cargo build --locked 2>&1 | tail -15`
Expected: compiles. (Behavior is HW-gated.)

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(randr): apply_crtc_config backend method (enable/disable/mode-change)

ModeSpec carries resolved (w,h,vrefresh). KMS enable allocates scanout +
commit_modeset at client (x,y); disable frees scanout + removes from active
set. Registry tracks current config + client_configured. No recompact on
client-driven config. Failure leaves prior consistent state.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

### Task 4.2: `RRSetCrtcConfig` handler — validation matrix

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs:2454` (split `RR_SET_CRTC_CONFIG` out of the no-op arm into a real handler)
- Modify: `crates/yserver-core/src/randr.rs` (validation helpers: resolve output→crtc, mode lookup)
- Test: `crates/yserver-core/src/randr.rs` (validation matrix)

- [ ] **Step 1: Write the failing validation-matrix tests**

In `randr.rs`, add two pure functions matching Xorg's validation order (`rrcrtc.c`: arity/output/mode → rotation → bounds). `validate_set_crtc_config` does arity + output-resolution + mode-resolution only (NO bounds); the handler does the rotation check, then calls `screen_encompasses` for the bounds check. This ordering matters: a request that is both bad-output and out-of-bounds must report the output error (Xorg validates outputs first).

```rust
    /// Validate arity + output/mode resolution for SetCrtcConfig (Xorg
    /// rrcrtc.c order, EXCLUDING rotation + bounds — the handler does
    /// those after, in that order). `Ok(None)` = disable;
    /// `Ok(Some(mode))` = enable with this resolved mode;
    /// `Err((code, error_value))` = protocol error + field-specific
    /// `errorValue`.
    pub fn validate_set_crtc_config(
        &self,
        crtc_id: u32,
        mode_id: u32,
        outputs: &[u32],
    ) -> Result<Option<crate::randr::RandrMode>, (u8, u32)>;

    /// Bounds check: does `mode` placed at `(x, y)` fit the current
    /// (logical) screen? Xorg `rrcrtc.c`: `x + width > screen.width` ⇒
    /// BadValue(errorValue=x); then `y + height > screen.height` ⇒
    /// BadValue(errorValue=y).
    pub fn screen_encompasses(
        &self,
        mode: &crate::randr::RandrMode,
        x: i16,
        y: i16,
    ) -> Result<(), (u8, u32)>;
```

Tests (error codes: `BAD_MATCH`, `BAD_VALUE` from `yserver_protocol::x11::error`). The `Err` tuple's second element is the offending field value:

```rust
    fn one_output_state() -> RandrState {
        RandrState::from_outputs_with_modes(
            1,
            vec![RandrOutput {
                name: "eDP-1".into(), output_id: 1, crtc_id: 2, mode_id: 7,
                connected: true, x: 0, y: 0, width: 1920, height: 1080, vrefresh: 60,
                mm_width: 0, mm_height: 0, mode_ids: vec![7, 8], num_preferred: 1,
            }],
            vec![
                RandrMode { mode_id: 7, width: 1920, height: 1080, vrefresh: 60 },
                RandrMode { mode_id: 8, width: 1280, height: 720, vrefresh: 60 },
            ],
        )
    }

    #[test]
    fn set_crtc_config_mode_none_with_outputs_is_badmatch() {
        let st = one_output_state();
        assert_eq!(st.validate_set_crtc_config(2, 0, &[1]), Err((x11_err::BAD_MATCH, 2)));
    }

    #[test]
    fn set_crtc_config_mode_set_with_no_outputs_is_badmatch() {
        let st = one_output_state();
        assert_eq!(st.validate_set_crtc_config(2, 7, &[]), Err((x11_err::BAD_MATCH, 2)));
    }

    #[test]
    fn set_crtc_config_output_not_driving_crtc_is_badmatch() {
        let st = one_output_state();
        // crtc 999 isn't this output's crtc → errorValue = the bad crtc.
        assert_eq!(st.validate_set_crtc_config(999, 7, &[1]), Err((x11_err::BAD_MATCH, 999)));
    }

    #[test]
    fn set_crtc_config_mode_not_in_output_list_is_badmatch() {
        let st = one_output_state();
        // bad mode → errorValue = the bad mode id.
        assert_eq!(st.validate_set_crtc_config(2, 555, &[1]), Err((x11_err::BAD_MATCH, 555)));
    }

    #[test]
    fn screen_encompasses_rejects_overflow_x_then_y() {
        let st = one_output_state(); // screen 1920x1080
        let m1080 = RandrMode { mode_id: 7, width: 1920, height: 1080, vrefresh: 60 };
        // Place 1920x1080 at x=100 → 2020 > 1920. errorValue = x.
        assert_eq!(st.screen_encompasses(&m1080, 100, 0), Err((x11_err::BAD_VALUE, 100)));
        // x ok, y overflow: at y=100 → 1180 > 1080. errorValue = y.
        assert_eq!(st.screen_encompasses(&m1080, 0, 100), Err((x11_err::BAD_VALUE, 100)));
        // exact fit (x+w == screen.width) is allowed (Xorg uses `>`).
        assert_eq!(st.screen_encompasses(&m1080, 0, 0), Ok(()));
    }

    #[test]
    fn set_crtc_config_valid_enable_resolves_mode() {
        let st = one_output_state();
        let mode = st.validate_set_crtc_config(2, 8, &[1]).expect("valid").expect("enable");
        assert_eq!((mode.width, mode.height), (1280, 720));
    }

    #[test]
    fn set_crtc_config_valid_disable() {
        let st = one_output_state();
        assert_eq!(st.validate_set_crtc_config(2, 0, &[]), Ok(None));
    }
```

Add `use yserver_protocol::x11::error as x11_err;` to the test module.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p yserver-core set_crtc_config_ -- --nocapture`
Expected: FAIL to compile (`validate_set_crtc_config` missing).

- [ ] **Step 3: Implement `validate_set_crtc_config`**

In `randr.rs`, matching Xorg `rrcrtc.c` order:

```rust
    pub fn validate_set_crtc_config(
        &self,
        crtc_id: u32,
        mode_id: u32,
        outputs: &[u32],
    ) -> Result<Option<RandrMode>, (u8, u32)> {
        use yserver_protocol::x11::error;
        // 1. mode/outputs arity. errorValue = the addressed crtc.
        if mode_id == 0 {
            if !outputs.is_empty() {
                return Err((error::BAD_MATCH, crtc_id));
            }
            return Ok(None); // disable
        }
        if outputs.is_empty() {
            return Err((error::BAD_MATCH, crtc_id));
        }
        // 2. outputs resolve to known connectors AND drive this crtc (1:1).
        for &oid in outputs {
            let Some(out) = self.outputs.iter().find(|o| o.output_id == oid) else {
                return Err((error::BAD_MATCH, oid)); // unknown output
            };
            if out.crtc_id != crtc_id {
                return Err((error::BAD_MATCH, crtc_id)); // output doesn't drive crtc
            }
        }
        // The addressed crtc must belong to one of the named outputs.
        let out = self
            .outputs
            .iter()
            .find(|o| o.crtc_id == crtc_id && outputs.contains(&o.output_id))
            .ok_or((error::BAD_MATCH, crtc_id))?;
        // 3. mode ∈ this output's advertised list. errorValue = bad mode.
        if !out.mode_ids.contains(&mode_id) {
            return Err((error::BAD_MATCH, mode_id));
        }
        let mode = self
            .mode_table
            .iter()
            .find(|m| m.mode_id == mode_id)
            .copied()
            .ok_or((error::BAD_MATCH, mode_id))?;
        Ok(Some(mode))
    }

    pub fn screen_encompasses(
        &self,
        mode: &RandrMode,
        x: i16,
        y: i16,
    ) -> Result<(), (u8, u32)> {
        use yserver_protocol::x11::error;
        // Xorg rrcrtc.c: `x + width > screen.width` ⇒ BadValue(x), then
        // `y + height > screen.height` ⇒ BadValue(y). errorValue carries
        // the raw INT16 sign-extended into the CARD32 field.
        if i32::from(x) + i32::from(mode.width) > i32::from(self.screen_width) {
            return Err((error::BAD_VALUE, i32::from(x) as u32));
        }
        if i32::from(y) + i32::from(mode.height) > i32::from(self.screen_height) {
            return Err((error::BAD_VALUE, i32::from(y) as u32));
        }
        Ok(())
    }
```

(Make `RandrMode` derive `Copy, PartialEq` so `.copied()` and the test asserts work.)

- [ ] **Step 4: Run the matrix tests**

Run: `cargo test -p yserver-core set_crtc_config_`
Expected: all PASS.

- [ ] **Step 5: Replace the no-op handler arm**

In `process_request.rs:2454`, the arm handles `RR_SET_SCREEN_CONFIG | RR_SET_CRTC_CONFIG` together. Split `RR_SET_CRTC_CONFIG` into its own arm (keep `RR_SET_SCREEN_CONFIG` as the RANDR-1.0 no-op accept — MATE's legacy restore path still uses it). Body layout: `crtc(4) timestamp(4) config_timestamp(4) x(2) y(2) mode(4) rotation(2) pad(2) outputs(4n)`.

Xorg-faithfulness points the handler must honor (codex, verified against `rrcrtc.c:1300-1500`):

1. **Validation order is arity/output/mode → rotation → bounds.** `rrcrtc.c` does NOT validate `config_timestamp`/`timestamp` against the server's last times in SetCrtcConfig — there is **no** InvalidConfigTime/InvalidTime gating here (those statuses exist for the legacy 1.0 `SetScreenConfig`, not 1.2 `SetCrtcConfig`). Do not add it.
2. **Rotation (after output/mode validation):** rotation is a non-goal. Two distinct rejections matching Xorg: if `rotation & 0xf` is not one of `{1,2,4,8}` (RR_Rotate_0/90/180/270) ⇒ `BadValue(errorValue=rotation)`; else if `rotation != RR_Rotate_0` (1) ⇒ `BadMatch(errorValue=rotation)` (our identity-only CRTC doesn't support it — Xorg's `(~crtc->rotations) & rotation` check). The rotation/bounds checks only apply when enabling (`mode != None`).
3. **Timestamp:** on success Xorg sets `lastSetTime = ClientTimeToServerTime(stuff->timestamp)` and replies `newTimestamp = lastSetTime`. So the reply timestamp is derived from the **client's** request timestamp (CurrentTime/0 ⇒ server now), NOT `timestamp_now()` taken before. A CRTC set bumps `lastSetTime` but **NOT** `lastConfigTime` (the available config didn't change). On failure (`RRSetConfigFailed`), `lastSetTime` is unchanged and the reply carries the existing value.
4. **Error values** are field-specific (`validate_set_crtc_config`/`screen_encompasses` return `(code, error_value)`).

```rust
                x11randr::RR_SET_CRTC_CONFIG => {
                    let crtc = u32::from_le_bytes(body.get(0..4)?.try_into()?);
                    let req_timestamp = u32::from_le_bytes(body.get(4..8)?.try_into()?);
                    // config_timestamp at body[8..12] is parsed but NOT
                    // validated (rrcrtc.c does not gate on it).
                    let x = i16::from_le_bytes(body.get(12..14)?.try_into()?);
                    let y = i16::from_le_bytes(body.get(14..16)?.try_into()?);
                    let mode = u32::from_le_bytes(body.get(16..20)?.try_into()?);
                    let rotation = u16::from_le_bytes(body.get(20..22)?.try_into()?);
                    let outputs: Vec<u32> = body.get(24..).map(parse_u32_array).unwrap_or_default();

                    // (1) arity + output + mode resolution FIRST.
                    let resolved = match state.randr.validate_set_crtc_config(crtc, mode, &outputs) {
                        Err((code, error_value)) => {
                            return emit_x11_error_with_minor(
                                state, client_id, sequence, code,
                                error_value, u16::from(header.data), RANDR_MAJOR_OPCODE,
                            );
                        }
                        Ok(r) => r,
                    };
                    // (2)+(3) rotation + bounds only when enabling.
                    if let Some(ref m) = resolved {
                        if !matches!(rotation & 0xf, 1 | 2 | 4 | 8) {
                            return emit_x11_error_with_minor(
                                state, client_id, sequence, x11::error::BAD_VALUE,
                                u32::from(rotation), u16::from(header.data), RANDR_MAJOR_OPCODE,
                            );
                        }
                        if rotation != 1 {
                            return emit_x11_error_with_minor(
                                state, client_id, sequence, x11::error::BAD_MATCH,
                                u32::from(rotation), u16::from(header.data), RANDR_MAJOR_OPCODE,
                            );
                        }
                        if let Err((code, error_value)) = state.randr.screen_encompasses(m, x, y) {
                            return emit_x11_error_with_minor(
                                state, client_id, sequence, code,
                                error_value, u16::from(header.data), RANDR_MAJOR_OPCODE,
                            );
                        }
                    }

                    let connector = state.randr.outputs.iter()
                        .find(|o| o.crtc_id == crtc)
                        .map(|o| o.name.clone());
                    let Some(connector) = connector else {
                        return emit_x11_error_with_minor(
                            state, client_id, sequence, x11::error::BAD_MATCH,
                            crtc, u16::from(header.data), RANDR_MAJOR_OPCODE,
                        );
                    };
                    let mode_spec = resolved.map(|m| ModeSpec {
                        width: m.width, height: m.height, vrefresh: m.vrefresh,
                    });
                    // lastSetTime = client timestamp (0/CurrentTime ⇒ now).
                    let set_time = if req_timestamp == 0 { state.timestamp_now() } else { req_timestamp };
                    match backend.apply_crtc_config(
                        &connector, mode_spec, i32::from(x), i32::from(y),
                    ) {
                        Ok(()) => {
                            // Single rebuild path: a CRTC set bumps lastSetTime
                            // (to the client time) but NOT lastConfigTime.
                            backend.refresh_randr_state_set_time(state, set_time);
                            let changed: Vec<(u32,u32,u32)> = state.randr.outputs.iter()
                                .find(|o| o.name == connector)
                                .map(|o| (o.output_id, o.crtc_id, o.mode_id))
                                .into_iter().collect();
                            // SetCrtcConfig fires Crtc/Output change (+ ScreenChange
                            // via RRTellChanged) → emit_randr_change_notifications.
                            emit_randr_change_notifications(state, &changed);
                            reply_set_crtc_config(state, client_id, sequence, byte_order, 0, state.randr.timestamp)
                        }
                        Err(e) => {
                            log::warn!("RRSetCrtcConfig apply failed: {e}");
                            // RRSetConfigFailed=3 (status reply, not error).
                            // lastSetTime unchanged → reply the existing value.
                            reply_set_crtc_config(state, client_id, sequence, byte_order, 3, state.randr.timestamp)
                        }
                    }
                }
```

Add a small `reply_set_crtc_config(state, client_id, sequence, byte_order, status, new_timestamp)` helper near the handler that writes the 32-byte reply (`status` in the data byte + `new_timestamp(4)` + `pad(20)`) and `write_to_client`, returning `io::Result<RequestOutcome>`.

This needs `Backend::refresh_randr_state_set_time(&mut self, state, set_time: u32)` which calls the consolidated `rebuild_randr_state(state, Some(set_time), false)` (KMS) — set-time bumped to the client timestamp, config-time unchanged; default no-op for ynest/recording. Add it to the trait. `ModeSpec` and `byte_order` are already in scope.

(`parse_u32_array` reads consecutive `u32`s from the tail; if no such helper exists, inline a `body[24..].chunks_exact(4).map(|c| u32::from_le_bytes(...))` collect.)

- [ ] **Step 6: Update the stale no-op SetCrtcConfig test**

A pre-existing test (`crates/yserver-core/src/core_loop/process_request.rs:~32971`) asserts the **old** no-op-accept behavior of `RR_SET_CRTC_CONFIG` (status 0 for a known mode, status 3 otherwise). The real handler changes that contract. Find it:

Run: `rg -n "RR_SET_CRTC_CONFIG|SetCrtcConfig|no-op accept" crates/yserver-core/src/core_loop/process_request.rs | rg -i "test|assert|3297"`

Rewrite it to assert the new behavior (valid enable → status 0 + the changed-state rebuild; bad mode → `BadMatch`; bad rotation → `BadValue`; disable → status 0). Keep the `RR_SET_SCREEN_CONFIG` (RANDR 1.0) no-op-accept test as-is — that path is unchanged.

- [ ] **Step 7: Build + run all tests**

Run: `cargo build --locked 2>&1 | tail -10 && cargo test -p yserver-core`
Expected: clean / PASS.

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "feat(randr): RRSetCrtcConfig real handler — validation matrix + apply

Replaces the no-op accept. Validates arity/ownership/mode/encompass per
rrcrtc.c (BadMatch/BadValue), calls apply_crtc_config, replies
Success/Failed, fires Crtc/Output change notifies. SetScreenConfig (RANDR
1.0) stays a no-op accept.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Phase 5 — Off-until-configured hotplug + preservation across rescan/resume

Goal: a runtime-connected monitor stays dark until a client enables it; rescan/resume never flattens client-set geometry.

### Task 5.1: Stop unconditional recompact; preserve `client_configured` outputs

**Files:**
- Modify: `crates/yserver/src/kms/v2/platform.rs:2363` (`requery_outputs_and_modeset` — drop unconditional `recompact_horizontal_layout` :2351 call; re-apply registry config)
- Modify: `crates/yserver/src/kms/v2/platform.rs:2285` (`dpms_set_outputs_active` — re-commit registry mode, already mostly does)

- [ ] **Step 1: Make recompact skip `client_configured` outputs**

In `recompact_horizontal_layout` (`:2351`), only lay out outputs whose registry entry has `client_configured == false`; leave configured ones at their stored `(x, y)`. Since `recompact` is on `PlatformBackend` but the registry lives on `KmsBackendV2.randr_id_alloc`, move the preservation decision to the caller, or pass a `&HashMap<String, bool>` of client-configured flags. Simplest: change `recompact_horizontal_layout` to take the registry view:

```rust
    fn recompact_horizontal_layout(&mut self, is_client_configured: &dyn Fn(&str) -> bool) {
        let mut next_x: i32 = 0;
        // First, leave client-configured outputs where they are; only
        // pack the auto-layout (default) ones into the gap after them.
        for layout in &mut self.outputs {
            if is_client_configured(&layout.output.connector_name) {
                continue;
            }
            layout.x = next_x;
            layout.y = 0;
            next_x = next_x.saturating_add(i32::from(layout.width));
        }
    }
```

(Refine the gap logic during impl if client+auto outputs coexist; the common case is "all auto" at boot or "all client" after MATE configures.)

- [ ] **Step 2: Drop the unconditional recompact from the rescan path; re-apply registry config**

In `requery_outputs_and_modeset` (`:2363`, the tail near the old `:2351` recompact call), replace the unconditional `self.recompact_horizontal_layout();` with: re-apply each surviving enabled output's registry `Enabled { x, y, mode }` to its `OutputLayout`, and run the (now-guarded) recompact only over `!client_configured` outputs. New connectors are added **off** (Task 5.2), so they don't get auto-placed here.

Since `requery_outputs_and_modeset` is on `PlatformBackend` and needs registry access, either pass the registry in or split the recompact/re-apply step up to `KmsBackendV2` where both `platform` and `randr_id_alloc` are reachable. Recommended: add a `KmsBackendV2::reconcile_layout_from_registry(&mut self)` that, after `platform.requery_outputs_and_modeset()` returns, walks `platform.outputs` and sets each `client_configured` output's `(x,y)` from the registry, then calls the guarded recompact and `recompute_fb_extent_from`.

- [ ] **Step 3: Build**

Run: `cargo build --locked 2>&1 | tail -10`
Expected: compiles.

- [ ] **Step 4: Add a core-level preservation test (registry-independent)**

The position-preservation invariant is observable in core: after a client sets an output to `(0, 1080)` and a rescan rebuilds `state.randr`, the output keeps `(0,1080)`. Since the registry is backend-side, test the core contract instead: that `from_outputs_with_modes` faithfully reflects whatever positions the bridge supplies (it does — no recompute of x/y). Add a backend-level integration note in the HW gate (Task 6) for the true end-to-end check. Write a core test asserting `from_outputs` does not zero a non-zero `y`:

```rust
    #[test]
    fn from_outputs_preserves_nonzero_y_position() {
        let outs = vec![RandrOutput {
            name: "HDMI-A-1".into(), output_id: 1, crtc_id: 2, mode_id: 3,
            connected: true, x: 0, y: 1080, width: 1920, height: 1080, vrefresh: 60,
            mm_width: 0, mm_height: 0, mode_ids: vec![3], num_preferred: 1,
        }];
        let st = RandrState::from_outputs(0, outs);
        assert_eq!(st.outputs[0].y, 1080, "y must not be flattened");
        assert_eq!(st.crtc_info(2, 0).unwrap().y, 1080);
    }
```

Run: `cargo test -p yserver-core from_outputs_preserves_nonzero_y`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(randr): preserve client-configured layout across rescan/resume

recompact_horizontal_layout skips client_configured outputs; the rescan
path re-applies registry config instead of unconditional extend-right.
VT-resume re-light already re-commits the stored mode.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

### Task 5.2: Hotplug-add registers off (no auto-enable)

**Files:**
- Modify: `crates/yserver/src/kms/v2/platform.rs:2420-2522` (rescan add-path) and `backend.rs` `fire_randr_changes` / hotplug handling

- [ ] **Step 1: Register newly-connected connectors off**

In the rescan add-path (`requery_outputs_and_modeset`, lines ~2420–2522 that currently allocate a scanout pool + add to `platform.outputs` for a newly-connected connector), change it so a brand-new connector is registered in the registry as `connected = true, config = Off`, its mode list recorded, but it is **NOT** added to `platform.outputs` and gets **no** scanout pool or modeset. This matches Xorg: `output->crtc = NULL` until a client enables it. The boot path (Task: initial configuration) keeps auto-enabling already-connected outputs.

Distinguish boot from runtime hotplug: the boot enable happens in the initial `open_with_commit`/first discovery; `requery_outputs_and_modeset` is the *runtime rescan* path → it should register-off. Verify boot still auto-enables (the initial setup does not go through the runtime rescan).

Run: `rg -n "requery_outputs_and_modeset|open_with_commit|fn open|initial" crates/yserver/src/kms/v2/platform.rs crates/yserver/src/kms/v2/backend.rs | head`
to confirm boot vs rescan are distinct entry points before editing.

- [ ] **Step 2: Fire `OutputChangeNotify` on hotplug-add (no Crtc/Screen change)**

In `fire_randr_changes` (`backend.rs:4889`), for an added-but-off connector, the notify must reflect `crtc=0, mode=0` (off). Since `fire_randr_changes` rebuilds `state.randr` from `randr_outputs_and_modes` (which now reports the off connector with `mode_id=0`), the existing fanout works — confirm the `changed` tuple uses the off connector's `(output_id, 0, 0)`.

- [ ] **Step 3: Build**

Run: `cargo build --locked 2>&1 | tail -10`
Expected: compiles.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "feat(randr): hotplug-add registers output off until client-configured

Runtime-connected monitor flips connection state + fires OutputChangeNotify
but stays dark (crtc=None) until RRSetCrtcConfig enables it. Matches Xorg
rrinfo.c:173 / rroutput.c:293. Boot still auto-enables connected outputs.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Phase 6 — Verification gate

### Task 6.1: Full pre-commit gate + regression sweep

- [ ] **Step 1: Format + clippy + full test**

Run:
```bash
cargo fmt
cargo clippy 2>&1 | rg -i "warning|error" | head -40
cargo test 2>&1 | tail -30
```
Expected: no new warnings, all tests pass. Fix any fallout before proceeding.

- [ ] **Step 2: ynest regression (no behavior change there)**

Run: `just xts-yserver` (vng iteration signal per memory `feedback_vng_pass_not_hw_pass`) and confirm RANDR-related XTS categories are unchanged. The nested backend uses default no-op trait impls, so single-monitor behavior must be identical.

- [ ] **Step 3: Commit any cleanup**

```bash
git add -A
git commit -m "chore(randr): fmt + clippy + test gate green for output-management set

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

### Task 6.2: HW acceptance on fuji/MATE (user-run — coordinate per `feedback_hw_recipes_user_only`)

This is the real gate. Hand to the user to run on fuji with MATE. Each step is observed visually.

- [ ] **1. Extend at 1080p:** `xrandr --output HDMI-A-1 --mode 1920x1080 --right-of eDP-1` → HDMI lights at 1080p, MATE paints wallpaper on it, windows drag across the seam.
- [ ] **2. Resolution change via MATE Display settings** applies and persists.
- [ ] **3. Disable laptop panel:** `xrandr --output eDP-1 --off` → eDP goes dark, HDMI keeps working; re-enable restores it.
- [ ] **4. Hotplug while idle** → the monitor stays dark until configured (Xorg-faithful), then `xrandr --auto` / MATE lights it.
- [ ] **5. Regression:** single-monitor MATE unchanged; VT-switch away/back still re-lights (resume fix intact); a `just rendercheck-yserver-hw` / XTS pass shows no new failures.

Capture `yserver-hw-mate.log` during each step (RUST_LOG=info on the apply paths) and grep for EINVAL atomic-commit storms (cf. memory `project_einval_atomic_commit_storm_wedge`) and reclamation-starvation symptoms when an output is disabled (cf. `project_reclamation_starvation_leak`).

### Task 6.3: Finish the branch

- [ ] Use `superpowers:finishing-a-development-branch` to decide integration (PR to master — master is branch-protected per memory `project_master_protected`; no direct push). Draft the PR body and **get explicit approval before opening** (memory `feedback_draft_public_comms_first`).

---

## Self-review notes (spec coverage)

- Mode lists preferred-first + `nPreferred` → Tasks 1.1, 2.1.
- 2-D bounding box (`max(y+height)`) → Task 1.2; non-zero-y preservation → 5.1.
- Connector registry + `client_configured` → Task 1.4; preservation rule → 5.1.
- `GetScreenResources` reprobe vs `GetScreenResourcesCurrent` cached → Task 2.2.
- `GetOutputInfo`/`GetCrtcInfo` off/disconnected reporting → existing `output_info`/`crtc_info` already handle `connected=false`/`mode_id=0`; mode list added in 2.1. (Verify `crtc_info` returns zeroed geometry for an off output — it reads `out.x/width/...` which are 0 for off; confirm during 2.1.)
- `GetMonitors`/XINERAMA enabled-only → Task 2.3.
- `RRSetScreenSize` (range/crop/zero-mm, ConfigureNotify, fanout, pointer warp) → Tasks 3.1, 3.2.
- `RRSetCrtcConfig` validation matrix + apply + Failed-not-error → Tasks 4.1, 4.2.
- Off-until-configured hotplug → Task 5.2.
- Backend methods `apply_crtc_config` / `set_logical_screen_size` / `reprobe_connectors` / `refresh_randr_state_set_time` (+ no-op defaults for ynest/recording) → Tasks 2.2, 3.1, 4.1, 4.2; all rebuilds route through the single `rebuild_randr_state(state, set_time, config_changed)` helper → 2.2 Step 1.
- 2-D `recompute_fb_extent_from` (`platform.rs:623`, `max(y+height)`) → **Task 1.5** (moved into Phase 1 — Phase 4's enable path depends on it).
- Protocol-crate `RR_SET_SCREEN_SIZE` opcode + parser → **Task 3.0** (prerequisite; the const did not exist).
- RANDR timestamp semantics (request `timestamp`/`config_timestamp` validation → InvalidTime/InvalidConfigTime status; `newTimestamp` = post-set lastSetTime; reprobe bumps config_timestamp only on change) → Tasks 2.2, 4.2.
- Rotation rejection (identity-only, non-goal) → Task 4.2.

## Resolved review points (codex pass 1)

- **Single rebuild path:** one `rebuild_randr_state(state, set_time: Option<u32>, config_changed: bool)` (Task 2.2 Step 1) serves hotplug (`fire_randr_changes`), reprobe, and SetCrtcConfig — no divergent timestamp/notify drift. It carries the client-set screen size forward across every rebuild.
- **Screen-size notify shape:** `apply_screen_size_side_effects` takes the `changed` list as a param; `RRSetScreenSize` passes `&[]` so only `ScreenChangeNotify` (+ root `ConfigureNotify`) fires, matching Xorg `RRScreenSizeSet`.
- **Backend-failure error mapping:** `RRSetScreenSize` backend failure → `BadMatch`; `GetScreenResources` reprobe failure → `BadAlloc`; `RRSetCrtcConfig` DDX failure → status `RRSetConfigFailed=3` (reply, not error).

## Open questions / risks (from spec — resolve during impl)

- **COW vs root storage on resize** (Task 3.1): confirm `set_logical_screen_size` resizes whichever backs the logical screen (comp-on = COW, comp-off = root+windows).
- **Reconfigure bursts:** each request applied + replied synchronously; position-only changes skip scanout realloc (only genuine resolution changes pay drain+realloc). No coalescing.
- **Timestamp model fidelity:** yserver's `timestamp_now()` is monotonic server time; confirm `state.randr.timestamp`/`config_timestamp` advance only through the consolidated rebuild so the InvalidTime/InvalidConfigTime comparisons in Task 4.2 are meaningful (a DE that round-trips GetScreenResources → SetCrtcConfig must see consistent values).
