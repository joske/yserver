# RANDR CRTC Gamma Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `redshift`/`gammastep`/Night Light work on yserver by implementing RANDR `GetCrtcGammaSize`/`GetCrtcGamma`/`SetCrtcGamma` against the KMS backend, persisted across modeset/VT-switch/DPMS.

**Architecture:** Three layers mirroring the existing `SetCrtcConfig` flow — core RANDR handlers (`process_request.rs`) resolve a RANDR CRTC id to a connector name and call new `Backend` trait methods; the KMS backend owns a per-connector gamma LUT cache, applies it via the legacy `drm` `set_gamma` ioctl, and reapplies it after every successful `commit_modeset`.

**Tech Stack:** Rust; `drm` crate 0.15 (`Device::set_gamma`/`get_gamma`, CRTC `gamma_length`); yserver `Backend` trait; X11 RANDR wire protocol.

**Spec:** `docs/superpowers/specs/2026-06-18-randr-gamma-design.md` (approved, codex-reviewed ×2).

**Reconciliation vs spec (discovered during planning, codex-corrected):** the spec says invalid CRTC → `BadCrtc`. yserver *does* publish a nonzero RANDR `first_error` (`nested.rs:35`/`:145`), so a real `RRBadCrtc` (= `first_error + 1`) is technically available. **But** every existing RANDR handler — notably the sibling `GetCrtcInfo` (`process_request.rs:2293/2304`) — returns core `x11::error::BAD_VALUE` for an invalid CRTC. To stay internally consistent with the established RANDR handler behaviour (and avoid a one-off divergence), this plan uses **`BAD_VALUE`** for invalid CRTC in all three gamma handlers, matching `GetCrtcInfo`. The SetCrtcGamma-specific codes (`BadLength`, `BadMatch`) and the leased-CRTC `BadAccess` seam are kept per spec. A real client (redshift) never sends an invalid CRTC, so this is invisible to the target app. Converting *all* RANDR handlers to real `RRBadCrtc` is a separate, deliberate cleanup — not this feature's job.

---

## File Structure

- `crates/yserver-core/src/backend/trait_def.rs` — 3 new `Backend` trait methods with safe defaults.
- `crates/yserver-core/src/backend/recording.rs` — `RecordingBackend` gamma impl (in-memory, for unit tests) + the shared `gamma_identity_ramp` / `resample_gamma` helpers live here-adjacent (see Task 2).
- `crates/yserver-protocol/src/x11/randr.rs` — `RR_SET_CRTC_GAMMA` const; rewrite `encode_get_crtc_gamma_reply` to emit the R/G/B arrays + correct length.
- `crates/yserver-protocol/src/x11/request_swap.rs` — add a RANDR (major 128) byte-swap table with the `RR_SET_CRTC_GAMMA` entry.
- `crates/yserver-core/src/core_loop/process_request.rs` — wire `GetCrtcGammaSize`/`GetCrtcGamma` to the backend; add the `RR_SET_CRTC_GAMMA` dispatch arm.
- `crates/yserver/src/kms/v2/backend.rs` + `platform.rs` — KMS cache, the 3 trait impls, the connector→crtc resolution, and the reapply-after-`commit_modeset` hooks.
- `crates/yserver-core/src/backend/gamma.rs` (new) — pure `gamma_identity_ramp` + `resample_gamma` helpers shared by RecordingBackend and KMS, unit-tested in isolation.

---

## Task 1: Gamma helpers (pure functions)

**Files:**
- Create: `crates/yserver-core/src/backend/gamma.rs`
- Modify: `crates/yserver-core/src/backend/mod.rs` (add `pub mod gamma;` and re-export)

- [ ] **Step 1: Write the failing test**

Create `crates/yserver-core/src/backend/gamma.rs`:

```rust
//! Pure gamma-LUT helpers shared by every backend: the linear identity
//! ramp used to seed a CRTC's cache, and the resample used when a
//! connector lands on a hardware CRTC of a different gamma size.

/// A linear identity ramp of `size` entries: `entry[i] = i * 65535 /
/// (size-1)`, so a fresh CRTC reports neutral gamma (matches Xorg's
/// initial ramp). `size == 0` → empty; `size == 1` → `[0]`.
#[must_use]
pub fn identity_ramp(size: u16) -> Vec<u16> {
    let n = usize::from(size);
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![0];
    }
    (0..n)
        .map(|i| u16::try_from(i as u64 * 65535 / (n as u64 - 1)).unwrap_or(u16::MAX))
        .collect()
}

/// Resample one channel from `src.len()` to `dst_len` entries by linear
/// interpolation, preserving endpoints. `dst_len == 1` → `[src[0]]`;
/// `src.len() == 1` → `dst_len` copies of `src[0]`; empty `src` → empty.
#[must_use]
pub fn resample_channel(src: &[u16], dst_len: usize) -> Vec<u16> {
    if src.is_empty() || dst_len == 0 {
        return Vec::new();
    }
    if dst_len == 1 {
        return vec![src[0]];
    }
    if src.len() == 1 {
        return vec![src[0]; dst_len];
    }
    let src_max = src.len() - 1;
    let denom = (dst_len - 1) as u64;
    (0..dst_len)
        .map(|i| {
            // pos in [0, src_max], exact at the endpoints. All-u64 so an
            // arbitrary (not just gamma-domain) size can't overflow.
            let num = i as u64 * src_max as u64;
            let lo = (num / denom) as usize;
            let rem = num % denom;
            if rem == 0 || lo >= src_max {
                return src[lo.min(src_max)];
            }
            let a = u64::from(src[lo]);
            let b = u64::from(src[lo + 1]);
            // a + (b-a) * rem/(dst_len-1), rounded to nearest.
            let interp = (a * (denom - rem) + b * rem + denom / 2) / denom;
            u16::try_from(interp).unwrap_or(u16::MAX)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_ramp_endpoints_and_size() {
        assert_eq!(identity_ramp(0), Vec::<u16>::new());
        assert_eq!(identity_ramp(1), vec![0]);
        let r = identity_ramp(256);
        assert_eq!(r.len(), 256);
        assert_eq!(r[0], 0);
        assert_eq!(r[255], 65535);
        assert_eq!(r[128], (128 * 65535 / 255) as u16);
    }

    #[test]
    fn resample_preserves_endpoints_and_handles_degenerate() {
        let src = identity_ramp(256);
        let down = resample_channel(&src, 16);
        assert_eq!(down.len(), 16);
        assert_eq!(down[0], 0);
        assert_eq!(down[15], 65535);
        assert_eq!(resample_channel(&src, 1), vec![0]);
        assert_eq!(resample_channel(&[7], 4), vec![7, 7, 7, 7]);
        assert_eq!(resample_channel(&[], 4), Vec::<u16>::new());
        // identical size is a no-op round-trip.
        assert_eq!(resample_channel(&src, 256), src);
    }
}
```

- [ ] **Step 2: Wire the module**

In `crates/yserver-core/src/backend/mod.rs`, add `pub mod gamma;` next to the other `pub mod` lines.

- [ ] **Step 3: Run test to verify it passes**

Run: `cargo test -p yserver-core --lib backend::gamma`
Expected: PASS (2 tests).

- [ ] **Step 4: Commit**

```bash
git add crates/yserver-core/src/backend/gamma.rs crates/yserver-core/src/backend/mod.rs
git commit -m "feat(randr-gamma): pure identity-ramp + resample helpers"
```

---

## Task 2: Backend trait methods + RecordingBackend impl

**Files:**
- Modify: `crates/yserver-core/src/backend/trait_def.rs` (after `apply_crtc_config`, ~line 367)
- Modify: `crates/yserver-core/src/backend/recording.rs` (struct field + impl block + `new()`)

- [ ] **Step 1: Add the trait methods (with safe defaults)**

In `trait_def.rs`, immediately after the `apply_crtc_config` method's closing brace (~`:367`):

```rust
    /// Number of entries in the connector's CRTC hardware gamma LUT
    /// (`0` = gamma unsupported on this backend/connector). Default: 0.
    fn crtc_gamma_size(&self, _connector: &str) -> u16 {
        0
    }

    /// Cache the gamma LUT for `connector` (cache-before-apply: store
    /// the requested ramp first), then apply it to the live CRTC. Each
    /// of `red`/`green`/`blue` has `crtc_gamma_size` entries. A transient
    /// apply failure keeps the cached ramp so a later reapply retries it.
    /// Default: Ok(()) no-op.
    fn set_crtc_gamma(
        &mut self,
        _connector: &str,
        _red: &[u16],
        _green: &[u16],
        _blue: &[u16],
    ) -> io::Result<()> {
        Ok(())
    }

    /// The connector's current cached gamma LUT (lazily seeded with a
    /// linear identity ramp on first query/set). Default: empty triple.
    fn get_crtc_gamma(&self, _connector: &str) -> (Vec<u16>, Vec<u16>, Vec<u16>) {
        (Vec::new(), Vec::new(), Vec::new())
    }
```

- [ ] **Step 2: Write the failing RecordingBackend test**

In `recording.rs`, inside the existing `#[cfg(test)] mod tests` (or the crate's test module that constructs `RecordingBackend`), add:

```rust
#[test]
fn recording_backend_gamma_roundtrip_and_seed() {
    use crate::backend::Backend;
    let mut b = RecordingBackend::new();
    // Reports a fixed size.
    assert_eq!(b.crtc_gamma_size("DP-1"), 256);
    // First get lazily seeds a linear identity ramp.
    let (r, g, bl) = b.get_crtc_gamma("DP-1");
    assert_eq!(r.len(), 256);
    assert_eq!(r[0], 0);
    assert_eq!(r[255], 65535);
    assert_eq!((g[255], bl[255]), (65535, 65535));
    // Set then get round-trips.
    let red = vec![1u16; 256];
    let green = vec![2u16; 256];
    let blue = vec![3u16; 256];
    b.set_crtc_gamma("DP-1", &red, &green, &blue).unwrap();
    assert_eq!(b.get_crtc_gamma("DP-1"), (red, green, blue));
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p yserver-core --lib recording_backend_gamma_roundtrip_and_seed`
Expected: FAIL (default `crtc_gamma_size` returns 0, not 256).

- [ ] **Step 4: Implement the RecordingBackend gamma impl**

Add a field to the `RecordingBackend` struct (after `warped_to`):

```rust
    /// In-memory per-connector gamma LUT for unit tests (size 256).
    pub gamma: std::cell::RefCell<std::collections::HashMap<String, (Vec<u16>, Vec<u16>, Vec<u16>)>>,
```

Initialise it in `RecordingBackend::new()` (after `warped_to: None,`):

```rust
            gamma: std::cell::RefCell::new(std::collections::HashMap::new()),
```

Add the trait method overrides to RecordingBackend's `impl Backend` block:

```rust
    fn crtc_gamma_size(&self, _connector: &str) -> u16 {
        256
    }

    fn set_crtc_gamma(
        &mut self,
        connector: &str,
        red: &[u16],
        green: &[u16],
        blue: &[u16],
    ) -> io::Result<()> {
        self.gamma.borrow_mut().insert(
            connector.to_string(),
            (red.to_vec(), green.to_vec(), blue.to_vec()),
        );
        Ok(())
    }

    fn get_crtc_gamma(&self, connector: &str) -> (Vec<u16>, Vec<u16>, Vec<u16>) {
        if let Some(lut) = self.gamma.borrow().get(connector) {
            return lut.clone();
        }
        let ramp = crate::backend::gamma::identity_ramp(256);
        (ramp.clone(), ramp.clone(), ramp)
    }
```

(`RefCell` because `get_crtc_gamma` takes `&self` but must lazily seed; the test thread has exclusive access.)

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p yserver-core --lib recording_backend_gamma_roundtrip_and_seed`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/yserver-core/src/backend/trait_def.rs crates/yserver-core/src/backend/recording.rs
git commit -m "feat(randr-gamma): Backend gamma trait methods + RecordingBackend impl"
```

---

## Task 3: Protocol — `RR_SET_CRTC_GAMMA` const + real `GetCrtcGamma` reply

**Files:**
- Modify: `crates/yserver-protocol/src/x11/randr.rs` (const near `:52`; rewrite `encode_get_crtc_gamma_reply` at `:768`)

- [ ] **Step 1: Write the failing encoder test**

In `randr.rs` tests module, add:

```rust
#[test]
fn get_crtc_gamma_reply_emits_arrays_and_length() {
    let red = vec![10u16, 20, 30];
    let green = vec![40u16, 50, 60];
    let blue = vec![70u16, 80, 90];
    let out = encode_get_crtc_gamma_reply(ClientByteOrder::LittleEndian, SequenceNumber(7), &red, &green, &blue);
    // size@8
    assert_eq!(u16::from_le_bytes([out[8], out[9]]), 3);
    // length (32-bit units) = (3*3*2 + 3) >> 2 = (18+3)>>2 = 5
    assert_eq!(u32::from_le_bytes([out[4], out[5], out[6], out[7]]), 5);
    // header 32 bytes + 5*4 = 20 payload bytes = 52, padded.
    assert_eq!(out.len(), 32 + 5 * 4);
    // arrays contiguous from byte 32: red, green, blue.
    let vals: Vec<u16> = out[32..32 + 18]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    assert_eq!(vals, vec![10, 20, 30, 40, 50, 60, 70, 80, 90]);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p yserver-protocol get_crtc_gamma_reply_emits_arrays_and_length`
Expected: FAIL (signature mismatch — current encoder takes `size: u16`, not arrays).

- [ ] **Step 3: Add the const + rewrite the encoder**

Near `:52` add:

```rust
pub const RR_SET_CRTC_GAMMA: u8 = 24;
```

Replace `encode_get_crtc_gamma_reply` (`:768`) with:

```rust
/// Encodes a `GetCrtcGamma` reply: 32-byte header with `size`@8, then a
/// contiguous `red|green|blue` block of `u16`, zero-padded to 4 bytes.
/// `reply.length` (32-bit units) = `(3 * size * 2 + 3) >> 2` — the header
/// is counted separately (mirrors Xorg rrcrtc.c:1693/1704).
#[must_use]
pub fn encode_get_crtc_gamma_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    red: &[u16],
    green: &[u16],
    blue: &[u16],
) -> Vec<u8> {
    debug_assert_eq!(red.len(), green.len());
    debug_assert_eq!(green.len(), blue.len());
    let size = u16::try_from(red.len()).unwrap_or(0);
    let payload_bytes = usize::from(size) * 3 * 2;
    let length_units = u32::try_from((payload_bytes + 3) >> 2).unwrap_or(0);
    // fixed_reply(byte_order, sequence, data: u8, length: u32) writes the
    // 8-byte reply prefix only (data@byte1, length@4..8); caller pads to 32.
    let mut out = fixed_reply(byte_order, sequence, 0, length_units);
    put(byte_order, &mut out, size); // bytes 8-9: size
    out.extend_from_slice(&[0u8; 22]); // bytes 10-31: pad
    debug_assert_eq!(out.len(), 32);
    for ch in [red, green, blue] {
        for &v in ch {
            put(byte_order, &mut out, v);
        }
    }
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out
}
```

- [ ] **Step 4: Run the protocol test in isolation**

Run: `cargo test -p yserver-protocol get_crtc_gamma_reply_emits_arrays_and_length`
Expected: PASS.

- [ ] **Step 5: DO NOT COMMIT YET — this changes `encode_get_crtc_gamma_reply`'s signature, which breaks the existing core callsite at `process_request.rs:2460`. The workspace stays uncompilable until Task 5 updates that callsite. Proceed to Task 4 (independent) then Task 5, and commit the encoder + handlers together in Task 5's commit.** (codex review: don't leave a non-building commit.)

---

## Task 4: Byte-swap table for `RR_SET_CRTC_GAMMA`

**Files:**
- Modify: `crates/yserver-protocol/src/x11/request_swap.rs` (`extension_request_swap_table` at `:51`)

- [ ] **Step 1: Write the failing test**

In `request_swap.rs` tests, add:

```rust
#[test]
fn randr_set_crtc_gamma_swaps_crtc_size_and_array() {
    // The swap layer converts an inbound BIG-ENDIAN client body into
    // native (LE) form. So start with BE bytes (what a BE client sends:
    // crtc=0x01020304, size=3, then 3 u16 = 1,2,3) and assert LE after.
    let mut body = vec![0x01, 0x02, 0x03, 0x04, 0x00, 0x03, 0x00, 0x00,
                        0x00, 0x01, 0x00, 0x02, 0x00, 0x03];
    swap_request_body(128, super::super::randr::RR_SET_CRTC_GAMMA, ClientByteOrder::BigEndian, &mut body);
    // crtc now native LE (0x01020304)
    assert_eq!(&body[0..4], &[0x04, 0x03, 0x02, 0x01]);
    // size now native LE (3)
    assert_eq!(&body[4..6], &[0x03, 0x00]);
    // array u16s now native LE (1,2,3)
    assert_eq!(&body[8..14], &[0x01, 0x00, 0x02, 0x00, 0x03, 0x00]);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p yserver-protocol randr_set_crtc_gamma_swaps`
Expected: FAIL (major 128 currently returns `None` → no swap).

- [ ] **Step 3: Add the RANDR swap table**

In `extension_request_swap_table`, add a `128` arm:

```rust
        // RANDR extension (major fixed at 128).
        128 => randr_request_swap_table(minor),
```

Add the table function next to `xi_request_swap_table`:

```rust
const fn randr_request_swap_table(minor: u8) -> Option<&'static [FieldEntry]> {
    use FieldEntry::{ElementArrayTail, Fixed};
    use FieldKind::{U16, U32};
    match minor {
        // 24 SetCrtcGamma: crtc(u32) size(u16) pad(u16) red|green|blue(u16[])
        24 => Some(&[
            Fixed { offset: 0, kind: U32 },
            Fixed { offset: 4, kind: U16 },
            ElementArrayTail { from: 8, kind: U16 },
        ]),
        _ => None,
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p yserver-protocol randr_set_crtc_gamma_swaps`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver-protocol/src/x11/request_swap.rs
git commit -m "feat(randr-gamma): byte-swap table for RANDR SetCrtcGamma"
```

---

## Task 5: Core RANDR handlers

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` (`:2451` GetCrtcGammaSize, `:2459` GetCrtcGamma, new `RR_SET_CRTC_GAMMA` arm)

Connector resolution helper (used by all three) mirrors `SetCrtcConfig` (`:2722`):
`state.randr.outputs.iter().find(|o| o.crtc_id == crtc).map(|o| o.name.clone())`.

- [ ] **Step 1: Write failing handler tests**

Add to the `process_request` test module (mirror the existing RANDR handler tests; `crtc_id` 2 is the seeded output per `randr.rs:155`). Helper to drive a RANDR request through the dispatcher already exists for `SetCrtcConfig` tests — reuse it (`randr_set_crtc_config_validates_mode_id` at `:34639` shows the harness). Tests:

```rust
#[test]
fn rr_get_crtc_gamma_size_reports_backend_size() {
    // RecordingBackend reports 256.
    // dispatch RR_GET_CRTC_GAMMA_SIZE { crtc = 2 } -> reply size@8 == 256.
}
#[test]
fn rr_get_crtc_gamma_seeds_identity() {
    // dispatch RR_GET_CRTC_GAMMA { crtc = 2 } before any set ->
    // size@8 == 256, red[0]==0, red[255]==65535.
}
#[test]
fn rr_set_then_get_crtc_gamma_roundtrips() {
    // SetCrtcGamma { crtc=2, size=256, ramp } then GetCrtcGamma -> same ramp.
}
#[test]
fn rr_set_crtc_gamma_wrong_size_is_bad_match() {
    // SetCrtcGamma { crtc=2, size=128, but gamma_size=256 } -> BadMatch.
}
#[test]
fn rr_set_crtc_gamma_short_body_is_bad_length() {
    // SetCrtcGamma { crtc=2, size=256, but only 8 u16 supplied } -> BadLength.
}
#[test]
fn rr_gamma_invalid_crtc_is_bad_value() {
    // GetCrtcGammaSize/GetCrtcGamma/SetCrtcGamma { crtc = 999 } -> BadValue
    // (matches sibling GetCrtcInfo convention).
}
#[test]
fn rr_get_crtc_gamma_short_body_is_bad_length() {
    // GetCrtcGammaSize / GetCrtcGamma with body shorter than crtc(4) ->
    // BadLength (Xorg REQUEST_SIZE_MATCH), not BadValue.
}
```

Fill each test body using the same dispatch harness the existing RANDR tests use (construct the `RequestHeader { opcode: RANDR_MAJOR_OPCODE, data: <RR minor>, length_units }`, byte body, call the dispatcher, read the client buffer). For the error tests assert `bytes[0]==0` and `bytes[1]==x11::error::BAD_MATCH` / `BAD_LENGTH` / `BAD_VALUE`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p yserver-core --lib rr_get_crtc_gamma rr_set`
Expected: FAIL (handlers still hardcoded; SetCrtcGamma unhandled → falls through).

- [ ] **Step 3: Implement the three handlers**

Replace the `RR_GET_CRTC_GAMMA_SIZE` arm (`:2451`):

```rust
        x11randr::RR_GET_CRTC_GAMMA_SIZE => {
            // Xorg REQUEST_SIZE_MATCH: fixed body crtc(4) must be present
            // → short request is BadLength, not BadValue (codex).
            if body.len() < 4 {
                return emit_x11_error_with_minor(state, client_id, sequence,
                    x11::error::BAD_LENGTH, 0, u16::from(header.data), RANDR_MAJOR_OPCODE);
            }
            let crtc = body
                .get(0..4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
                .unwrap_or(0);
            let Some(connector) = state.randr.outputs.iter()
                .find(|o| o.crtc_id == crtc).map(|o| o.name.clone()) else {
                return emit_x11_error_with_minor(state, client_id, sequence,
                    x11::error::BAD_VALUE, crtc, u16::from(header.data), RANDR_MAJOR_OPCODE);
            };
            let size = backend.crtc_gamma_size(&connector);
            let buf = x11randr::encode_get_crtc_gamma_size_reply(byte_order, sequence, size);
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            return Ok(write_to_client(client, client_id, &buf));
        }
```

Replace the `RR_GET_CRTC_GAMMA` arm (`:2459`):

```rust
        x11randr::RR_GET_CRTC_GAMMA => {
            // Xorg REQUEST_SIZE_MATCH: fixed body crtc(4) must be present
            // → short request is BadLength, not BadValue (codex).
            if body.len() < 4 {
                return emit_x11_error_with_minor(state, client_id, sequence,
                    x11::error::BAD_LENGTH, 0, u16::from(header.data), RANDR_MAJOR_OPCODE);
            }
            let crtc = body
                .get(0..4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
                .unwrap_or(0);
            let Some(connector) = state.randr.outputs.iter()
                .find(|o| o.crtc_id == crtc).map(|o| o.name.clone()) else {
                return emit_x11_error_with_minor(state, client_id, sequence,
                    x11::error::BAD_VALUE, crtc, u16::from(header.data), RANDR_MAJOR_OPCODE);
            };
            let (red, green, blue) = backend.get_crtc_gamma(&connector);
            let buf = x11randr::encode_get_crtc_gamma_reply(byte_order, sequence, &red, &green, &blue);
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            return Ok(write_to_client(client, client_id, &buf));
        }
```

Add a new arm (place it next to the two above):

```rust
        x11randr::RR_SET_CRTC_GAMMA => {
            // Body: crtc(4) size(2) pad(2) red|green|blue (u16[size]).
            // (0) fixed body must be present FIRST — Xorg's
            // REQUEST_AT_LEAST_SIZE(xRRSetCrtcGammaReq) runs before
            // VERIFY_RR_CRTC, so a grossly short request is BadLength, not
            // BadValue. crtc(4)+size(2)+pad(2)=8 body bytes ⇒ length_units≥3.
            // (Required because we read crtc/size with unwrap_or(0) below.)
            if header.length_units < 3 || body.len() < 8 {
                return emit_x11_error_with_minor(state, client_id, sequence,
                    x11::error::BAD_LENGTH, 0, u16::from(header.data), RANDR_MAJOR_OPCODE);
            }
            let crtc = body
                .get(0..4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
                .unwrap_or(0);
            let size = body
                .get(4..6)
                .map(|b| u16::from_le_bytes(b.try_into().unwrap()))
                .unwrap_or(0);
            // (1) invalid CRTC -> BadValue (sibling GetCrtcInfo convention).
            let Some(connector) = state.randr.outputs.iter()
                .find(|o| o.crtc_id == crtc).map(|o| o.name.clone()) else {
                return emit_x11_error_with_minor(state, client_id, sequence,
                    x11::error::BAD_VALUE, crtc, u16::from(header.data), RANDR_MAJOR_OPCODE);
            };
            // (2) leased CRTC -> BadAccess. yserver has no lease state yet.
            if crtc_is_leased(crtc) {
                return emit_x11_error_with_minor(state, client_id, sequence,
                    x11::error::BAD_ACCESS, crtc, u16::from(header.data), RANDR_MAJOR_OPCODE);
            }
            // (3) short body -> BadLength: length_units - 3 < ((3*size+1)>>1).
            let fixed_units: u32 = 3;
            let need = (u32::from(size) * 3 + 1) >> 1;
            if header.length_units.saturating_sub(fixed_units) < need {
                return emit_x11_error_with_minor(state, client_id, sequence,
                    x11::error::BAD_LENGTH, 0, u16::from(header.data), RANDR_MAJOR_OPCODE);
            }
            // (4) size mismatch -> BadMatch.
            if size != backend.crtc_gamma_size(&connector) {
                return emit_x11_error_with_minor(state, client_id, sequence,
                    x11::error::BAD_MATCH, u32::from(size), u16::from(header.data), RANDR_MAJOR_OPCODE);
            }
            // Parse the three contiguous u16 arrays from offset 8.
            let n = usize::from(size);
            let read_ch = |start: usize| -> Vec<u16> {
                (0..n).map(|i| {
                    let off = start + i * 2;
                    body.get(off..off + 2)
                        .map(|b| u16::from_le_bytes(b.try_into().unwrap()))
                        .unwrap_or(0)
                }).collect()
            };
            let red = read_ch(8);
            let green = read_ch(8 + n * 2);
            let blue = read_ch(8 + n * 4);
            if let Err(e) = backend.set_crtc_gamma(&connector, &red, &green, &blue) {
                log::warn!("RRSetCrtcGamma: backend set_crtc_gamma({connector}) failed: {e}");
            }
            return Ok(RequestOutcome::Handled); // void request, no reply.
        }
```

Add the lease seam near the other RANDR helpers in this file:

```rust
/// RANDR-lease check for the gamma path. yserver has no real lease state
/// yet (RRCreateLease is a stopgap), so this is always false — a named
/// seam to fill when leases land, not an omitted check.
fn crtc_is_leased(_crtc: u32) -> bool {
    false
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p yserver-core --lib rr_get_crtc_gamma rr_set rr_gamma`
Expected: PASS (all 6).

- [ ] **Step 5: Full build + fmt + clippy**

Run: `cargo build -p yserver-core && cargo +nightly fmt && cargo clippy -p yserver-core`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
# Includes Task 3's encoder + const (held back so the tree builds).
git add crates/yserver-protocol/src/x11/randr.rs crates/yserver-core/src/core_loop/process_request.rs
git commit -m "feat(randr-gamma): GetCrtcGamma reply encoder + wire all three handlers"
```

---

## Task 6: KMS backend implementation

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs` (cache field on the backend struct; the 3 trait impls)
- Modify: `crates/yserver/src/kms/v2/platform.rs` (reapply hook after `commit_modeset` at `:2426` and `:2721`; a `connector → live Crtc handle + gamma_length` resolver)

> Most of this task is HW-verified, not unit-tested (KMS needs `/dev/dri`). Keep logic minimal and lean on the pure helpers from Task 1.

- [ ] **Step 1: Add the cache field**

On the KMS backend struct (`backend.rs`, near the other steady-state caches), add:

```rust
    /// Per-connector gamma LUT cache (connector name, NOT crtc_id — a
    /// connector's hardware CRTC can change across re-enable). Source of
    /// truth for GetCrtcGamma and for reapply after commit_modeset.
    gamma_cache: std::collections::HashMap<String, (Vec<u16>, Vec<u16>, Vec<u16>)>,
```

Initialise `gamma_cache: std::collections::HashMap::new(),` in the backend constructor.

- [ ] **Step 2: Add a connector → (Crtc handle, gamma_length) resolver in `platform.rs`**

Mirror the existing connector lookup (`apply_crtc_config` uses `self.platform.outputs.iter().find(|l| l.output.connector_name == connector)`; the cursor code in `platform.rs:1049+` shows how a live CRTC handle is obtained and how `self.device` ioctls are called). Add:

```rust
    /// Resolve a connector name to its live DRM CRTC handle and the
    /// CRTC's legacy gamma ramp length. `None` if the connector is not
    /// currently lit (no CRTC bound).
    pub(crate) fn connector_gamma_target(
        &self,
        connector: &str,
    ) -> Option<(::drm::control::crtc::Handle, u16)> {
        let layout = self.outputs.iter().find(|l| l.output.connector_name == connector)?;
        let crtc = layout.output.crtc; // the bound CRTC handle
        let info = self.device.get_crtc(crtc).ok()?;
        Some((crtc, info.gamma_length() as u16))
    }
```

(Confirm the exact field for the bound CRTC handle on the output-layout struct and the `device` accessor by reading the cursor path in `platform.rs:1049–1160`; use the identical handle/`self.device` the cursor ioctls use.)

- [ ] **Step 3: Implement the 3 trait methods on the KMS backend (`backend.rs`)**

```rust
    fn crtc_gamma_size(&self, connector: &str) -> u16 {
        // A LIVE CRTC's reported size is authoritative — including 0, which
        // honestly means "gamma unsupported on this CRTC" (do NOT mask 0 as
        // 256, codex caught that). Only an unlit/known connector (no CRTC to
        // query → None) falls back to the conventional 256 so redshift can
        // allocate before the output lights up.
        match self.platform.connector_gamma_target(connector) {
            Some((_, len)) => len,
            None => 256,
        }
    }

    fn set_crtc_gamma(
        &mut self,
        connector: &str,
        red: &[u16],
        green: &[u16],
        blue: &[u16],
    ) -> io::Result<()> {
        // Cache-before-apply: store the requested ramp first so a transient
        // ioctl failure still gets retried on the next reapply.
        self.gamma_cache.insert(
            connector.to_string(),
            (red.to_vec(), green.to_vec(), blue.to_vec()),
        );
        if let Some((crtc, len)) = self.platform.connector_gamma_target(connector) {
            let (r, g, b) = resample_triple_to(red, green, blue, usize::from(len));
            self.platform.device.set_gamma(crtc, &r, &g, &b)?;
        }
        Ok(())
    }

    fn get_crtc_gamma(&self, connector: &str) -> (Vec<u16>, Vec<u16>, Vec<u16>) {
        if let Some(lut) = self.gamma_cache.get(connector) {
            return lut.clone();
        }
        let size = self.crtc_gamma_size(connector);
        let ramp = yserver_core::backend::gamma::identity_ramp(size);
        (ramp.clone(), ramp.clone(), ramp)
    }
```

Add a small local helper in `backend.rs`:

```rust
fn resample_triple_to(
    red: &[u16],
    green: &[u16],
    blue: &[u16],
    len: usize,
) -> (Vec<u16>, Vec<u16>, Vec<u16>) {
    use yserver_core::backend::gamma::resample_channel;
    if red.len() == len {
        return (red.to_vec(), green.to_vec(), blue.to_vec());
    }
    (
        resample_channel(red, len),
        resample_channel(green, len),
        resample_channel(blue, len),
    )
}
```

> Note: `get_crtc_gamma` takes `&self` but the cache is not lazily persisted here (unlike RecordingBackend). That is fine: an unset connector returns a freshly-computed identity ramp every call (cold path), and the first `SetCrtcGamma` populates the cache. If you prefer to persist the seed, change the signature plan in Task 2 — but YAGNI: identity-on-read is correct and stateless.

- [ ] **Step 4: Add the reapply hook after every successful `commit_modeset`**

Add a backend method:

```rust
    /// Re-push every cached gamma LUT to its connector's current CRTC.
    /// Call AFTER a successful commit_modeset (the modeset resets the HW LUT).
    pub(crate) fn reapply_all_gamma(&mut self) {
        let entries: Vec<String> = self.gamma_cache.keys().cloned().collect();
        for connector in entries {
            if let Some(lut) = self.gamma_cache.get(&connector).cloned() {
                if let Some((crtc, len)) = self.platform.connector_gamma_target(&connector) {
                    let (r, g, b) = resample_triple_to(&lut.0, &lut.1, &lut.2, usize::from(len));
                    if let Err(e) = self.platform.device.set_gamma(crtc, &r, &g, &b) {
                        log::warn!("reapply_all_gamma: set_gamma({connector}) failed: {e}");
                    }
                }
            }
        }
    }
```

Call `self.reapply_all_gamma()` at the **backend** boundary (only `KmsBackendV2` owns `gamma_cache`; `PlatformBackend` cannot). Codex pinned the three exact sites in `backend.rs`:
- **After `self.platform.enable_connector(...)` succeeds** (`backend.rs:10057`), before the scene rebuild/wake at `:10086` — covers RANDR modeset.
- **After `self.platform.dpms_set_outputs_active(true)` returns in resume** (`backend.rs:5055`), before cursor rearm — covers VT-switch resume.
- **After `let res = self.platform.dpms_set_outputs_active(true);` in DPMS wake** (`backend.rs:16341`), before `rearm_cursor` — covers DPMS-on.

On the two DPMS paths, run `reapply_all_gamma()` **even if the helper returned `Err`** — `dpms_set_outputs_active` is best-effort and may partially light outputs (`platform.rs:2693`). The invariant: after any path that (re)commits a modeset, before the next pageflip is scheduled. (These are three distinct sites — note the first codex review's "resume goes through dpms" was an over-simplification; resume at `:5055` and DPMS-wake at `:16341` are separate call sites and both need the hook.)

- [ ] **Step 5: Build**

Run: `cargo build -p yserver`
Expected: clean (resolve the exact `output.crtc` field / `self.device`/`self.platform.device` accessor names against the cursor code while doing so).

- [ ] **Step 6: fmt + clippy + full test**

Run: `cargo +nightly fmt && cargo clippy && cargo test -p yserver-core -p yserver-protocol`
Expected: clean; all unit tests pass.

- [ ] **Step 7: Commit**

```bash
git add crates/yserver/src/kms/v2/backend.rs crates/yserver/src/kms/v2/platform.rs
git commit -m "feat(randr-gamma): KMS gamma apply + connector cache + reapply on modeset"
```

---

## Task 7: HW smoke (release gate — manual, on silence/RX580)

> Not automatable (needs `/dev/dri` + a real display). This is the acceptance gate per the no-commit-before-smoke rule. Record results in the PR.

- [ ] **Step 1: Build release + run a session**

```bash
cargo build --release --bin yserver
```
Launch yserver on a TTY (e.g. via `just install` / the lightdm path) into Cinnamon.

- [ ] **Step 2: Gamma applies**

Run `redshift -O 3000` (or `gammastep -O 3000`). Expected: screen visibly warms. `redshift -x` → returns to neutral.
This directly fixes the confirmed 2026-06-18 baseline ("redshift does nothing on Cinnamon").

- [ ] **Step 3: Persistence across VT-switch**

With warm gamma applied, switch to another VT (Ctrl-Alt-F3) and back. Expected: gamma is still applied (reapply-after-modeset works), not reset to neutral.

- [ ] **Step 4: Legacy-gamma-under-pageflips (the open risk)**

While a normal composited desktop is actively repainting (e.g. play a video or drag a window), apply `redshift -O 3000`. Expected: the gamma apply succeeds without EBUSY storms / flicker. **If it collides:** do NOT switch to atomic GAMMA_LUT — instead sequence the `set_gamma` call on the next idle/vblank (still legacy ioctl). Record the outcome; this is the make-or-break validation from the spec's Open Questions.

- [ ] **Step 5: Record results** in the PR description (pass/fail per step + any EBUSY observations).

---

## Self-Review notes

- **Spec coverage:** trait methods (T2), KMS apply+cache+reapply (T6), connector-keyed cache (T6), error order BadValue/BadAccess/BadLength/BadMatch (T5 — BadCrtc→BadValue reconciliation documented in header), reply length formula (T3), swap entry (T4), identity seed + resample (T1/T6), persistence after commit_modeset (T6), redshift+VT-switch+pageflip smoke (T7). All covered.
- **Type consistency:** `crtc_gamma_size`/`set_crtc_gamma`/`get_crtc_gamma` signatures identical across trait (T2), RecordingBackend (T2), KMS (T6); `identity_ramp`/`resample_channel` names consistent (T1) and consumed unchanged in T2/T6; `encode_get_crtc_gamma_reply(byte_order, sequence, &red,&green,&blue)` consistent T3↔T5.
- **Codex plan review pass 2 folded in:** added the fixed-size `BadLength` guard to both GET handlers (Xorg `REQUEST_SIZE_MATCH`; the generic gate doesn't cover extensions, so the in-handler guard is the only one); flipped the Task-4 swap test to the correct direction (BE-client input → native-LE output, since the swap layer converts inbound client bytes to native); made the resample `num`/`lo`/`rem` fully `u64` so the math is overflow-proof for any size, not just the gamma domain. Codex confirmed the pass-1 fixes are all real (fixed_reply arg order, commit-boundary, SetCrtcGamma fixed-body guard, live-`0` policy, the three reapply sites, key-clone borrow safety).
- **Codex plan review (pass 1) folded in:** u64 intermediates in the gamma math (T1); `fixed_reply(byte_order, sequence, 0, length_units)` arg order + `ClientByteOrder::LittleEndian`/`BigEndian` (T3/T4); Task 3 held back to commit with Task 5 so no non-building commit; fixed-size `BadLength` guard *before* reading crtc (T5); `crtc_gamma_size` returns a live CRTC's `0` honestly (T6); the three exact reapply call sites + run-on-Err (T6 S4); reconciliation rationale corrected (RANDR *does* have a `first_error`; we match the sibling-handler `BAD_VALUE` convention).
- **Confirmed real by codex:** `output.crtc` (`kms/backend.rs:390` / `drm/modeset.rs:77`), `self.platform.device` (`platform.rs:504`), `emit_x11_error_with_minor` (`process_request.rs:16065`), the error constants, `FieldEntry`/`FieldKind` variants, and the drm 0.15 `get_crtc`/`gamma_length()`/`set_gamma` APIs. `connector_gamma_target`/`gamma_cache`/`reapply_all_gamma` are intentionally new names this plan introduces.
- **HW-verified (not unit-tested), flagged for the executor:** the resample-on-`gamma_size`-change *integration* (the pure resample is unit-tested in T1) and the partial-success DPMS-wake reapply — both exercised by the Task 7 smoke, not by `RecordingBackend`.
