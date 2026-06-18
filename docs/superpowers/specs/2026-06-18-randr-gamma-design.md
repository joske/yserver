# RANDR CRTC gamma — design

**Date:** 2026-06-18 · **Status:** approved, pre-plan · **Scope:** RANDR `GetCrtcGammaSize` / `GetCrtcGamma` / `SetCrtcGamma` (CRTC color LUT), KMS-backed, persisted across modeset/VT-switch/DPMS.

## Motivation

`redshift`, `gammastep`, and GNOME Night Light are flatly broken on yserver today: `GetCrtcGammaSize` returns `0` (`process_request.rs:2452`), `GetCrtcGamma` returns an empty ramp (`:2460`), and `SetCrtcGamma` is not dispatched at all. A client that asks for the gamma size gets `0` and either errors or no-ops. This is a real-client divergence from Xorg, the kind worth fixing (see `docs/superpowers/findings/2026-06-17-tech-debt-stub-audit.md`, Tier 4, and the re-prioritisation under the Xorg-baseline lens).

**Verification target is a real app, not XTS.** Real Xorg does not pass the XTS suites 100% (see memory `reference-xorg-not-100pct-on-xts-xi`), so the acceptance gate here is "redshift visibly warms the screen and the warmth survives a VT-switch", not an XTS verdict.

## Approach

Apply the LUT to hardware with the **legacy `drmModeCrtcSetGamma` ioctl**, not the atomic `GAMMA_LUT` property. Rationale: atomic commits on a CRTC EBUSY-collide with scanout pageflips — the exact hazard that drove the HW cursor to legacy ioctls (`drmModeMoveCursor`/`SetCursor2`, landed `0b3fd0c`, memory `feedback-hw-cursor-legacy-ioctls`). Gamma shares the hazard and gains nothing from atomic here.

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

/// The CRTC's current cached gamma LUT (seeded with a linear identity
/// ramp on first connector enable). Default: empty.
fn get_crtc_gamma(&self, connector: &str) -> (Vec<u16>, Vec<u16>, Vec<u16>) {
    (Vec::new(), Vec::new(), Vec::new())
}
```

The connector string is the same identifier `apply_crtc_config` already takes; core owns the RANDR-CRTC-id → connector resolution.

### 2. KMS implementation (`crates/yserver/src/kms/v2/{backend,platform}.rs`)

- **Cache:** the backend owns a `HashMap<crtc_id, GammaLut>` (`GammaLut = { red: Vec<u16>, green: Vec<u16>, blue: Vec<u16> }`). Hardware state lives with the backend, alongside the cursor sprite cache — not in core. This is a cold path (gamma changes are human-paced), so the extra backend round-trip for `GetCrtcGamma` is irrelevant.
- **`crtc_gamma_size`:** report the CRTC's real DRM `gamma_size` (the `drm` crate's CRTC info / `Crtc::gamma_length`). amdgpu legacy is typically 256; report whatever the kernel says so the client allocates the right array.
- **`set_crtc_gamma`:** call the `drm` crate's legacy `set_gamma(crtc, &red, &green, &blue)`. On success, store the LUT in the cache keyed by the resolved `crtc_id`.
- **`get_crtc_gamma`:** return the cached LUT (clone).
- **Seed:** on connector enable, if no cache entry exists, seed a linear identity ramp of length `gamma_size` (`entry[i] = (i * 65535 / (size-1))`), so a `GetCrtcGamma` before any `SetCrtcGamma` returns Xorg-like identity rather than empty.
- **Reapply:** after each transition that resets the hardware LUT, re-issue `set_gamma` from the cache for every CRTC with an entry. Hook sites (the same three the cursor rebind uses):
  - `enable_connector` → `commit_modeset` (`platform.rs:2426`)
  - `run_resume` (VT-switch return, `backend.rs:5010`)
  - `dpms_set_outputs_active(true)` (DPMS wake, `platform.rs:2721`)

### 3. RANDR protocol handlers (`crates/yserver-core/src/core_loop/process_request.rs`)

- **`RR_GET_CRTC_GAMMA_SIZE`** (`:2451`): resolve crtc → connector, return `backend.crtc_gamma_size(connector)` (replaces hardcoded `0`). Invalid CRTC → `BadCrtc`.
- **`RR_GET_CRTC_GAMMA`** (`:2459`): return `backend.get_crtc_gamma(connector)` as the reply ramp (replaces hardcoded empty). Invalid CRTC → `BadCrtc`.
- **`RR_SET_CRTC_GAMMA`** (new dispatch arm): parse `crtc(4) size(2) pad(2)` then `3 × size` `u16` entries. Validate `size == backend.crtc_gamma_size` → else `BadValue` (matches Xorg `rrcrtc.c ProcRRSetCrtcGamma`); invalid CRTC → `BadCrtc`. On success call `backend.set_crtc_gamma`. Void request (no reply).
- **Wire plumbing:** add the `RR_SET_CRTC_GAMMA` request-length entry to `request_lengths.rs` (fixed `crtc(4) size(2) pad(2)` + the `3 × size` `u16` data array, padded to 32-bit units — exact unit accounting to be derived against the existing RANDR entries in the plan, not hand-computed here) and the `request_swap.rs` entry (the `3 × size` array byte-swaps as `u16`).

## Error handling

- Invalid CRTC id → `BadCrtc` (RANDR error base).
- `SetCrtcGamma` size mismatch → `BadValue` (Xorg parity).
- KMS `set_gamma` ioctl failure → log a warning, leave the cache unchanged, and (for the protocol) still succeed-or-error per Xorg (Xorg returns Success once validation passes; a kernel failure is logged). We follow: validation errors are protocol errors; a post-validation ioctl failure is logged, not surfaced as a protocol error (no Xorg error code exists for it).
- Non-KMS backends report size 0 → clients that check `GetCrtcGammaSize` see "unsupported" and skip, which is honest.

## Testing

**Unit (`RecordingBackend`, in-memory ramp, size 256):**
- `GetCrtcGammaSize` returns the backend's size.
- `SetCrtcGamma` at the correct size stores; `GetCrtcGamma` round-trips the same ramp.
- `SetCrtcGamma` with `size != gamma_size` → `BadValue`.
- `GetCrtcGammaSize` / `GetCrtcGamma` / `SetCrtcGamma` on an invalid CRTC → `BadCrtc`.
- Seed: `GetCrtcGamma` before any `Set` returns a linear identity ramp.

**HW smoke (release gate — user-run on silence/RX580, per the no-commit-before-smoke rule):**
- `redshift -O 3000` → screen visibly warms; `redshift -x` → resets.
- Set gamma, switch to another VT and back → gamma persists (proves the reapply hook).

## Out of scope

Atomic `GAMMA_LUT`; per-output CTM / color-transform matrices; ICC/per-output color profiles; RANDR output color properties (`ListOutputProperties` etc. — tracked separately in the tech-debt doc). RANDR rotation/reflection is a separate, larger project (deferred this session).
