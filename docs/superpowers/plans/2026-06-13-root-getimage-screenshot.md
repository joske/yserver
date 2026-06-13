# Root-window GetImage (screenshot support) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make generic X11 screenshot tools (`xwd -root`, `scrot`, `import`, `xfwm4-screenshooter`) work against yserver instead of hanging.

**Architecture:** Two independent parts. **Part 1** (always needed) fixes the *hang*: a full-screen GetImage reply (~14 MiB) exceeds the per-client `OUTBOUND_CAP` and is dropped, and the disconnect path leaves a half-open socket. **Part 2** (gated on an empirical check) fixes *content*: root GetImage reads root storage (background only); the composite lives in the scanout BO, so root GetImage must read back the on-screen scanout.

**Tech Stack:** Rust, single-threaded mio/epoll core loop, non-blocking UNIX sockets, Vulkan (ash) + KMS/DRM.

**Spec:** `docs/superpowers/specs/2026-06-13-root-getimage-screenshot-design.md`

**Branch:** `feat/root-getimage-screenshot` (worktree `yserver-master`)

---

## Part 1 — deliver large replies + clean disconnect + byte-order (the hang)

### Task 1: Don't refuse a single legitimate reply at the outbound cap

The cap exists to disconnect a client that isn't *draining* (accumulating backlog), not to reject one large reply. When `outbound` is empty we're starting a fresh reply — buffer it in full regardless of size; the existing writable-interest machinery streams it out.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/client_io.rs` (`buffer_or_disconnect`)
- Test: same file (`#[cfg(test)]`)

- [ ] **Step 1: Write the failing test.** Call `buffer_or_disconnect` **directly** (it's private to the module; the `#[cfg(test)]` mod can call it) so the cap logic is exercised deterministically — going through `write_or_buffer` over a socketpair is flaky because a partial direct write would hit the remainder path instead. A single write larger than `OUTBOUND_CAP` onto an empty `outbound` must buffer in full and return `WouldBlock`; a write that pushes an *already non-empty* `outbound` past the cap must still return `Disconnect`.

```rust
#[test]
fn single_oversized_reply_buffers_when_outbound_empty() {
    let mut client = test_client(); // existing test ClientState ctor; outbound empty
    let big = vec![0u8; OUTBOUND_CAP + 1_000_000];
    let outcome = buffer_or_disconnect(&mut client, &big);
    assert_eq!(outcome, WriteOutcome::WouldBlock);
    assert_eq!(client.outbound.len(), big.len()); // buffered in full
}

#[test]
fn accumulation_past_cap_still_disconnects() {
    let mut client = test_client();
    client.outbound.extend(std::iter::repeat_n(0u8, OUTBOUND_CAP - 10));
    let outcome = buffer_or_disconnect(&mut client, &[0u8; 1000]);
    assert_eq!(outcome, WriteOutcome::Disconnect);
}
```

(Reuse the existing test `ClientState` constructor already used by the test at `client_io.rs:160`.)

- [ ] **Step 2: Run, verify failure.** `cargo test -p yserver-core single_oversized_reply_buffers_when_outbound_empty -- --nocapture` → FAIL (currently returns `Disconnect`).

- [ ] **Step 3: Implement.** In `buffer_or_disconnect`, allow a fresh reply through; apply the cap only to accumulation onto a non-empty buffer:

```rust
fn buffer_or_disconnect(client: &mut ClientState, bytes: &[u8]) -> WriteOutcome {
    // A single in-flight reply must never be refused: when outbound is
    // empty we're starting a fresh reply, so buffer it in full regardless
    // of size. The cap only guards against a client that isn't draining
    // (backlog piling onto an already-non-empty buffer).
    if !client.outbound.is_empty() && client.outbound.len() + bytes.len() > OUTBOUND_CAP {
        log::warn!(
            "client_io: outbound cap exceeded — outbound={} pending, +{} new ⇒ Disconnect",
            client.outbound.len(),
            bytes.len(),
        );
        return WriteOutcome::Disconnect;
    }
    client.outbound.extend(bytes.iter().copied());
    WriteOutcome::WouldBlock
}
```

- [ ] **Step 4: Run, verify pass.** Both tests pass; `cargo test -p yserver-core core_loop::client_io` green.

- [ ] **Step 5: Commit.** `fix(io): never refuse a single in-flight reply at the outbound cap (#21)`

---

### Task 2: Close the socket on disconnect (fixes the actual hang)

Disconnect currently sends `ReaderControl::Shutdown` and drops `ClientState`, but the reader thread blocked in `read_request` holds a cloned `UnixStream`, so the fd stays open and the peer never sees EOF — it hangs. Explicitly `shutdown(Shutdown::Both)` the socket so the kernel tears the connection down regardless of how many descriptors reference it.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_disconnect.rs` (the teardown path, ~lines 96 / 173)
- Test: same file or `run.rs`

- [ ] **Step 1: Write the failing test.** Build a connected socketpair as a client; drive a disconnect; assert the peer's `read()` returns `Ok(0)` (EOF) rather than `WouldBlock`/blocking.

```rust
#[test]
fn disconnect_shuts_down_socket_peer_sees_eof() {
    let (local, peer) = UnixStream::pair().unwrap();
    peer.set_nonblocking(true).unwrap();
    let mut state = test_state_with_client(local); // wraps `local` in ClientState
    disconnect_client(&mut state, ClientId(THE_ID)); // the teardown entry point
    let mut buf = [0u8; 1];
    // After shutdown(Both), the peer must observe EOF, not WouldBlock.
    assert!(matches!((&peer).read(&mut buf), Ok(0)));
}
```

- [ ] **Step 2: Run, verify failure.** Pre-fix the peer read returns `WouldBlock` (fd still open) → test FAILs.

- [ ] **Step 3: Implement.** In the disconnect teardown, before dropping the client, shut the socket down on both halves (ignore errors — the peer may already be gone):

```rust
// Force the kernel socket closed so a reader thread blocked in read_request
// unblocks and the peer sees EOF instead of a half-open hang (#21).
if let Ok(stream) = client.writer.lock() {
    let _ = stream.shutdown(std::net::Shutdown::Both);
}
```

(Place this at the teardown site that runs for `RequestOutcome::Disconnect` and connection-level disconnects alike.)

- [ ] **Step 4: Run, verify pass.** Test passes; `cargo test -p yserver-core process_disconnect` green.

- [ ] **Step 5: Commit.** `fix(io): shutdown(Both) on disconnect so peers see EOF, not a half-open hang (#21)`

---

### Task 3: Byte-order-correct GetImage reply header

The reply header's sequence, length, and visual are written little-endian unconditionally, which is wrong for big-endian clients — and **XTS connects big-endian clients**. `handle_get_image` already resolves `byte_order` and already post-patches the header in the backend (`Some(bytes)`) path, so fix it there (covers every backend uniformly; the `write_get_image_reply` fallback is already byte-order-aware).

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` (`handle_get_image`, the `Some(host_reply)` branch, ~line 18816)
- Test: `process_request.rs` `#[cfg(test)]` (or wherever GetImage reply tests live)

- [ ] **Step 1: Write the failing test.** Given a backend reply blob, the handler patches sequence (`[2..4]`), length (`[4..8]`), and visual (`[8..12]`) in the *client's* byte order. Assert with a big-endian client the bytes are big-endian.

```rust
#[test]
fn get_image_reply_header_honors_big_endian_client() {
    // 32-byte header + 8 bytes (2 px) of ZPixmap data.
    let bytes = build_backend_reply(/* depth */ 24, /* px_words */ 2);
    let patched = patch_get_image_header(bytes, ClientByteOrder::BigEndian,
                                         SequenceNumber(0x0102), ROOT_VISUAL.0);
    assert_eq!(&patched[2..4], &0x0102u16.to_be_bytes());
    assert_eq!(&patched[4..8], &2u32.to_be_bytes());          // length in words, BE
    assert_eq!(&patched[8..12], &ROOT_VISUAL.0.to_be_bytes());
}
```

- [ ] **Step 2: Run, verify failure.** FAILs (current patch is `to_le_bytes`, and length isn't patched at all).

- [ ] **Step 3: Implement.** Replace the LE patches in the `Some(bytes)` branch with byte-order-aware writes for sequence, **length** (recomputed from the payload so it always matches), and visual. Factor into a small `patch_get_image_header` helper so it's testable. Note: `yserver_protocol`'s `write_u16`/`write_u32` *append* to a `Vec` — they don't write in place at an offset — so patch with `copy_from_slice` of `to_le_bytes`/`to_be_bytes` selected by `order`:

```rust
fn patch_get_image_header(
    mut bytes: Vec<u8>,
    order: ClientByteOrder,
    sequence: SequenceNumber,
    visual: u32,
) -> Vec<u8> {
    if bytes.len() >= 32 {
        let len_words = ((bytes.len() - 32) / 4) as u32;
        let (seq, len, vis) = match order {
            ClientByteOrder::LittleEndian => (
                sequence.0.to_le_bytes(), len_words.to_le_bytes(), visual.to_le_bytes(),
            ),
            ClientByteOrder::BigEndian => (
                sequence.0.to_be_bytes(), len_words.to_be_bytes(), visual.to_be_bytes(),
            ),
        };
        bytes[2..4].copy_from_slice(&seq);
        bytes[4..8].copy_from_slice(&len);
        bytes[8..12].copy_from_slice(&vis);
    }
    bytes
}
```

(Pixel data is untouched — ZPixmap bytes stay in the server's advertised image-byte-order, which the client accounts for.)

- [ ] **Step 4: Run, verify pass.** Test green; `cargo test -p yserver-core handle_get_image` green.

- [ ] **Step 5: Commit.** `fix(getimage): write reply header in client byte order (XTS BE clients) (#21)`

---

### Task 4: Build, format, lint, and the empirical content gate

- [ ] **Step 1: Build + lint.** `cargo build --locked`, `cargo +nightly fmt`, `cargo clippy` (plain) — fix warnings in touched code.
- [ ] **Step 2: Full unit run.** `cargo test -p yserver-core` green.
- [ ] **Step 3: Empirical gate (the decision point for Part 2).** Run yserver (the live `:0` session is fine to rebuild against, or `just startx`), then:

```sh
xwd -root -silent -out /tmp/root.xwd && xwdtopnm /tmp/root.xwd > /tmp/root.pnm
```

`xwd` must now **return** (no hang). Inspect `/tmp/root.pnm`:
- **Shows the full desktop (windows present)** → root storage already reflects the composite in this configuration. **STOP — Part 2 is unnecessary.** Close #21 after smoke confirmation.
- **Shows wallpaper/background only (no windows)** → proceed to Part 2.

- [ ] **Step 4: Record the gate result** in the PR description and below before continuing.

---

## Part 2 — scanout readback for composited content (only if Task 4 shows wallpaper-only)

### Task 5: Refactor scanout readback into a reusable `read_scanout_region`

Extract the GPU→staging→CPU core of `do_dump_scanout_v2` into a function that copies an arbitrary root-relative rect. Critically, for screenshots it must select the **OnScreen BO only** (the dump's permissive `OnScreen→Pending→Submitted→Recording` fallback can return a torn/not-yet-visible frame).

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs` (`do_dump_scanout_v2` ~7328; add `read_scanout_region`)
- Test: same file `#[cfg(test)]`

- [ ] **Step 1: Write the failing test.** **Fixture note:** `for_tests_with_vk` has no scanout pools, and there is no synthetic multi-output fixture in-tree — so unit coverage here is limited to what `for_tests_with_vk_live_scene` provides (a single OnScreen BO). Write the feasible test: on the live-scene fixture, fill the OnScreen scanout BO with a known pattern; `read_scanout_region` of a sub-rect returns exactly that rect's bytes (packed `width*4` rows, BGRX). The "no OnScreen BO → `Err`" case is also unit-testable on a fixture with pools but no presented frame. The **multi-output spanning-rect → `Err`** case has no fixture without new plumbing — cover it in the HW smoke step (Task 7), or, only if unit coverage is required, add a minimal synthetic 2-pool fixture as a prerequisite sub-step. Do not block the implementer on a fixture that doesn't exist.

- [ ] **Step 2: Run, verify failure** (function doesn't exist yet).

- [ ] **Step 3: Implement.**

```rust
enum BoSelect { OnScreenOnly, PermissiveDump }

fn read_scanout_region(
    backend: &mut KmsBackendV2,
    rect: ash::vk::Rect2D,
    select: BoSelect,
) -> io::Result<Vec<u8>> {
    // Choose the BO: OnScreenOnly -> the OnScreen BO or Err; PermissiveDump
    // -> the existing OnScreen→Pending→Submitted→Recording fallback.
    // Copy only `rect` (BufferImageCopy image_offset/extent), staging sized
    // to rect, COPY-scoped barriers (not ALL_COMMANDS). Return packed
    // width*4 BGRX rows.
}
```

Then rewrite `do_dump_scanout_v2` to call `read_scanout_region(.., PermissiveDump)` with a full-BO rect and keep its PPM-writing tail.

- [ ] **Step 4: Run, verify pass.** Both tests green; the Ctrl-Alt-Enter PPM dump still works (manual or existing test).

- [ ] **Step 5: Commit.** `refactor(scanout): extract read_scanout_region (sub-rect, OnScreen-only) (#21)`

---

### Task 6: Route root GetImage to the scanout readback

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs` (`KmsBackendV2::get_image` ~11601)
- Test: same file

- [ ] **Step 1: Write the failing test.** On the live-scene fixture, a `get_image` whose original target is the root returns the scanout pixels (known pattern) rather than the background-only root storage. (Same fixture caveat as Task 5: the multi-output spanning-rect → `Ok(None)` assertion needs new fixture plumbing or HW gating — cover it in Task 7 rather than blocking here.)

- [ ] **Step 2: Run, verify failure.**

- [ ] **Step 3: Implement.** Detect the root/screen target from the **original** drawable *before* `resolve_paint_target` — check the store entry for the incoming `host_xid` is `DrawableKind::Root` (equivalently, `host_xid == self.core.window_id`). Do **not** classify from the resolved `PaintTarget`: `resolve_paint_target` can return a redirected *backing* when the root is redirected, which would misclassify a redirected root. When it's the root, source from `read_scanout_region(rect, OnScreenOnly)` instead of `engine.get_image`; run the bytes through the existing shared tail (`z_to_xy_planes`, `apply_z_plane_mask`, `wrap_get_image_reply`). On `Err` (no OnScreen BO, or multi-output spanning rect) return `Ok(None)` so the handler falls back to the existing reply. Single-output rects are served normally.

- [ ] **Step 4: Run, verify pass.**

- [ ] **Step 5: Commit.** `feat(getimage): root GetImage reads composited scanout (#21)`

---

### Task 7: HW smoke + regression

- [ ] **Step 1:** Rebuild; run yserver with a desktop. `xwd -root | xwdtopnm` → full desktop, no hang.
- [ ] **Step 2:** Region grab (`scrot -s` or `import` of a sub-rect) → the selected area, correct pixels.
- [ ] **Step 3:** Window grab (xwd-click a window) → that window (existing `engine.get_image` path — regression check).
- [ ] **Step 4:** Ctrl-Alt-Enter PPM dump still produces a correct image (shared `read_scanout_region`).
- [ ] **Step 5:** (If feasible) XTS GetImage subset, incl. a big-endian client, still passes.
- [ ] **Step 6: Commit** any test/recipe additions. Then open the PR for #21.

---

## Notes / deferred

- **Multi-head rect stitching** is a follow-up; a spanning rect returns `Ok(None)` (degrades) in the interim — never a silently-wrong partial capture.
- **Zero-size GetImage** keeps the existing handler `width.max(1)`/`height.max(1)` behaviour (1×1, not zero-length) — identical to the window path; a strict-conformance fix, if ever wanted, belongs once at the handler for all GetImage paths.
- **HW cursor** is a separate DRM plane and is intentionally absent from captures (matches Xorg).
- **MIT-SHM GetImage (`handle_mit_shm_get_image`, `process_request.rs:5144`) — FOLLOW-UP, out of scope for this branch.** OBS's default "Screen Capture (XSHM)" and `ffmpeg x11grab`+shm use `XShmGetImage`, which lands in this separate handler. It does *not* hit the OUTBOUND_CAP (pixel data goes to the shm segment, not the socket), so it won't hang — but it still routes root through `host_drawable_target` and reads background-only root storage, so a full-screen SHM capture shows wallpaper without windows. Fix = mirror Task 6's `root → read_scanout_region` routing in the SHM handler. Filed as a follow-up (2026-06-13); not blocking #21.
