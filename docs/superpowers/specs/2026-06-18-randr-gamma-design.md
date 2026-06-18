# RANDR CRTC gamma — design

**Date:** 2026-06-18 · **Status:** approved, codex-reviewed (2 passes), pre-plan · **Scope:** RANDR `GetCrtcGammaSize` / `GetCrtcGamma` / `SetCrtcGamma` (CRTC color LUT), KMS-backed, persisted across modeset/VT-switch/DPMS.

## Motivation

`redshift`, `gammastep`, and GNOME Night Light are flatly broken on yserver today: `GetCrtcGammaSize` returns `0` (`process_request.rs:2452`), `GetCrtcGamma` returns an empty ramp (`:2460`), and `SetCrtcGamma` is not dispatched at all. A client that asks for the gamma size gets `0` and either errors or no-ops. This is a real-client divergence from Xorg, the kind worth fixing (see `docs/superpowers/findings/2026-06-17-tech-debt-stub-audit.md`, Tier 4, and the re-prioritisation under the Xorg-baseline lens).

**Verification target is a real app, not XTS.** Real Xorg does not pass the XTS suites 100% (see memory `reference-xorg-not-100pct-on-xts-xi`), so the acceptance gate here is "redshift visibly warms the screen and the warmth survives a VT-switch", not an XTS verdict.

## Approach

Apply the LUT through the **legacy `drmModeCrtcSetGamma` ioctl** — the same userspace API `redshift`/`gammastep` use against Xorg, and consistent with the HW cursor's legacy-ioctl path (`drmModeMoveCursor`/`SetCursor2`, landed `0b3fd0c`, memory `feedback-hw-cursor-legacy-ioctls`). The point is that we do **not** add our own separate atomic `GAMMA_LUT` commit — a second atomic commit on a CRTC EBUSY-collides with our scanout pageflips (the hazard that drove the cursor to legacy ioctls).

**Caveat (codex review, verified against kernel headers):** on atomic-only drivers like amdgpu the legacy gamma ioctl is itself implemented via `drm_atomic_helper_legacy_gamma_set()` (`drm_crtc.h:507`), so it *may* internally route through the atomic color-management path. Whether that re-introduces an EBUSY/serialisation interaction with in-flight pageflips is **not proven** and is a genuine risk to the approach — it MUST be validated on target HW (see Open Questions / HW smoke). The legacy ioctl is still the right first choice: it's what redshift drives on Xorg, and the cursor uses the analogous legacy path successfully today.

## Architecture

Three layers, mirroring how `SetCrtcConfig` already flows (validate in core → resolve RANDR-CRTC-id to a connector → call the backend; dispatch at `process_request.rs:2749` via `backend.apply_crtc_config`).

### 1. `Backend` trait additions (`crates/yserver-core/src/backend/trait_def.rs`)

```rust
/// Number of entries in the CRTC's hardware gamma LUT (0 = gamma
/// unsupported on this backend/CRTC). Default: 0.
fn crtc_gamma_size(&self, connector: &str) -> u16 { 0 }

/// Apply a gamma LUT to the CRTC and cache it for reapply across
/// modeset / VT-switch / DPMS. `red`/`green`/`blue` each have
/// `crtc_gamma_size` entries. Default: Ok(()) no-op.
fn set_crtc_gamma(
    &mut self,
    connector: &str,
    red: &[u16],
    green: &[u16],
    blue: &[u16],
) -> std::io::Result<()> { Ok(()) }

/// The CRTC's current cached gamma LUT (lazily seeded with a linear
/// identity ramp on the connector's first gamma query/set). Default: empty.
fn get_crtc_gamma(&self, connector: &str) -> (Vec<u16>, Vec<u16>, Vec<u16>) {
    (Vec::new(), Vec::new(), Vec::new())
}
```

The connector string is the same identifier `apply_crtc_config` already takes; core owns the RANDR-CRTC-id → connector resolution.

### 2. KMS implementation (`crates/yserver/src/kms/v2/{backend,platform}.rs`)

- **Cache (keyed by CONNECTOR, not crtc_id):** the backend owns a `HashMap<connector, GammaLut>` (`GammaLut = { red: Vec<u16>, green: Vec<u16>, blue: Vec<u16> }`). **Codex caught this:** yserver's stable identity is the *connector*; the DRM `Output.crtc` a connector lands on can change across re-enable/rediscovery (`backend.rs:204`, `platform.rs:2835`), so a `crtc_id`-keyed cache would orphan a connector's LUT or reapply it to the wrong monitor. Cache by connector; resolve connector → current `crtc_id` at apply/reapply time. Hardware state lives with the backend, alongside the cursor sprite cache — not in core. Cold path (gamma changes are human-paced), so the extra backend round-trip for `GetCrtcGamma` is irrelevant.
- **`crtc_gamma_size`:** report the CRTC's real DRM `gamma_size` (the `drm` crate's CRTC info / `Crtc::gamma_length`). amdgpu legacy is typically 256; report whatever the kernel says so the client allocates the right array.
- **`set_crtc_gamma`:** store the LUT in the cache **keyed by connector** (see cache-before-apply in Error handling — the cache update precedes the ioctl), then resolve connector → current `crtc_id` and call the `drm` crate's legacy `set_gamma(crtc, &red, &green, &blue)`. The cache is connector-keyed throughout; `crtc_id` is only a transient lookup at apply time.
- **`get_crtc_gamma`:** return the cached LUT (clone).
- **Seed:** seed a linear identity ramp of length `gamma_size` (`entry[i] = i * 65535 / (size-1)`) the first time a connector's gamma is queried/set, so `GetCrtcGamma` before any `SetCrtcGamma` returns Xorg-like identity rather than empty. **Inactive/unassigned connectors:** yserver keeps a stable per-connector CRTC id even when the output is off (`backend.rs:2592`); `GetCrtcGammaSize`/`GetCrtcGamma`/`SetCrtcGamma` must still answer for those — report the connector's nominal gamma size (fall back to 256 when no live CRTC to query), cache the ramp, and apply lazily when the connector next lights up. If a connector lands on a hardware CRTC whose `gamma_size` differs from the cached ramp's length, **resample** the cached ramp to the new size on reapply (don't discard the client's intent). *Resample algorithm:* per channel, linear interpolation from `src_len` to `dst_len`, endpoints preserved (`dst[0]=src[0]`, `dst[dst_len-1]=src[src_len-1]`); for interior `i`, `pos = i * (src_len-1) / (dst_len-1)` (real), interpolate between `src[floor(pos)]` and `src[ceil(pos)]`, round to nearest `u16`. Degenerate cases: `dst_len==1` → `[src[0]]`; `src_len==1` → fill `dst` with `src[0]`.
- **Reapply — single invariant:** *re-issue `set_gamma` from the cache (resolving connector → current crtc) immediately after every successful `commit_modeset`.* **Codex caught the redundancy:** `run_resume` already re-lights outputs via `dpms_set_outputs_active(true)` (`backend.rs:5038`), which itself commits a modeset — so there is no separate top-level resume hook. The two real `commit_modeset` sites are `enable_connector` (`platform.rs:2426`, covers RANDR modeset) and DPMS wake (`platform.rs:2721`, covers DPMS-wake *and* VT-switch resume). Ordering: reapply strictly **after** the commit succeeds (a pre-commit apply would be wiped by the modeset).

### 3. RANDR protocol handlers (`crates/yserver-core/src/core_loop/process_request.rs`)

- **`RR_GET_CRTC_GAMMA_SIZE`** (`:2451`): resolve crtc → connector, return `backend.crtc_gamma_size(connector)` (replaces hardcoded `0`). Invalid CRTC → `BadCrtc`.
- **`RR_GET_CRTC_GAMMA`** (`:2459`): return `backend.get_crtc_gamma(connector)` as the reply ramp (replaces hardcoded empty). Invalid CRTC → `BadCrtc`. **Reply layout** (Xorg `rrcrtc.c:1693`): 32-byte reply header with `size`@byte 8, then one contiguous `3 × size` `u16` block (red, then green, then blue), zero-padded to a 4-byte boundary. `reply.length` (32-bit units) = `(3 × size × 2 + 3) >> 2` — spell this out exactly in the encoder.
- **`RR_SET_CRTC_GAMMA`** (new dispatch arm): parse `crtc(4) size(2) pad(2)` then `3 × size` `u16` entries. **Error order must match Xorg `ProcRRSetCrtcGamma` (rrcrtc.c:1723) exactly** — codex corrected my earlier `BadValue`:
  1. invalid CRTC → `BadCrtc` (`VERIFY_RR_CRTC`).
  2. CRTC is RANDR-leased → `BadAccess` (`RRCrtcIsLeased`). yserver has no lease state yet (RRCreateLease is a stopgap), so back this with an explicit `crtc_is_leased(crtc) -> bool` helper that returns `false` today — a named seam to fill when real leases land, not an omitted check.
  3. body too short → `BadLength`: `length_units - 3 < ((3 * size + 1) >> 1)` (the request's fixed part is 3 32-bit units — generic header + `crtc` + `size`/pad; this matches Xorg's `req_len - bytes_to_int32(sizeof(req))` check, rrcrtc.c:1736).
  4. `size != crtc gamma_size` → `BadMatch` (**not** `BadValue`).

  On success call `backend.set_crtc_gamma`. Void request (no reply).
- **Wire plumbing (codex corrected the location):**
  - The `BadLength` size check belongs **inside this handler**, in the order above — *not* in `request_lengths.rs`. That core gate runs *before* extension dispatch (`process_request.rs:111`) and intentionally passes majors ≥128 through unchecked (`validate_core_request_length(128, …) == true`); putting the check there would fire `BadLength` before `BadCrtc`/`BadAccess`, diverging from Xorg's order. So: no `request_lengths.rs` change.
  - **Byte-swap:** `request_swap.rs` currently has no RANDR table (only XI; `extension_request_swap_table` at `:51`). Add a RANDR (major **128**) entry for `RR_SET_CRTC_GAMMA`: `u32` at body offset 0 (`crtc`), `u16` at 4 (`size`), then a `u16[]` tail from offset 8. (Mirrors Xorg `rrsdispatch.c:325`, which swaps `crtc`, `size`, then the `CARD16` array.)

## Error handling

- Error codes/order per Xorg `ProcRRSetCrtcGamma` (see §3): `BadCrtc` → `BadAccess` (leased) → `BadLength` (short body) → `BadMatch` (size ≠ `gammaSize`). `Get*` invalid CRTC → `BadCrtc`.
- **Cache-before-apply (codex, Xorg parity):** on a valid `SetCrtcGamma`, update the connector's cached ramp to the client's requested values *first* (Xorg `RRCrtcGammaSet` copies into `crtc->gammaRed/Green/Blue` before invoking the driver hook, `rrcrtc.c:932`), *then* attempt the KMS `set_gamma`. A transient ioctl failure is logged (no Xorg protocol error exists for it) but the cache keeps the requested ramp, so the next reapply (or modeset) retries it. Returning Success after validation matches Xorg.
- Non-KMS backends report size 0 → clients that check `GetCrtcGammaSize` see "unsupported" and skip, which is honest.

## Testing

**Unit (`RecordingBackend`, in-memory ramp, size 256):**
- `GetCrtcGammaSize` returns the backend's size.
- `SetCrtcGamma` at the correct size stores; `GetCrtcGamma` round-trips the same ramp.
- `SetCrtcGamma` with `size != gamma_size` → `BadMatch`; with a body shorter than `size` implies → `BadLength`.
- `GetCrtcGammaSize` / `GetCrtcGamma` / `SetCrtcGamma` on an invalid CRTC → `BadCrtc`.
- Seed: `GetCrtcGamma` before any `Set` returns a linear identity ramp.
- Resample: covered by the pure `resample_channel` unit tests (no live CRTC needed).
- Reapply-after-`commit_modeset` is **KMS-only** (RecordingBackend has no modeset/commit hook to drive it), so it is **HW-verified** (VT-switch smoke), not unit-tested.

**HW smoke (release gate — user-run on silence/RX580, per the no-commit-before-smoke rule):**
- `redshift -O 3000` → screen visibly warms; `redshift -x` → resets.
- Set gamma, switch to another VT and back → gamma persists (proves the reapply hook).

## Open questions (codex)

- **Legacy gamma under active pageflips (must validate on HW):** does `redshift` reliably apply while normal scanout pageflips are running, or does `drm_atomic_helper_legacy_gamma_set` serialise/EBUSY against in-flight flips on amdgpu? This is the make-or-break for the legacy approach — it's the first thing the HW smoke checks. If it collides, fall back to scheduling the gamma apply on the next idle/vblank (still legacy ioctl, just sequenced), not to a separate atomic commit.
- **gamma_size change on CRTC remap:** resolved above — resample the cached ramp to the new size. Called out so the plan implements resampling, not silent truncation.

## Out of scope

Atomic `GAMMA_LUT`; per-output CTM / color-transform matrices; ICC/per-output color profiles; RANDR output color properties (`ListOutputProperties` etc. — tracked separately in the tech-debt doc). RANDR rotation/reflection is a separate, larger project (deferred this session).
