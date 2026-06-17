# XFIXES / XInput Pointer Barriers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement XFIXES `CreatePointerBarrier`/`DeletePointerBarrier`, XI2 `XIBarrierReleasePointer`, pointer confinement, and `BarrierHit`/`BarrierLeave` events — replacing the current silent no-op (the Tier-2 "advertised XFIXES v5 but can't deliver" gap).

**Architecture:** A `PointerBarrier` is a client-owned XID resource stored in a `HashMap` on `ServerState` (modeled on `XFixesRegion`). Pure clamp/segment math lives in a new `barriers.rs` module. The clamp hooks into `pointer_event_fanout_to_state` right after the existing confinement block. XI2 events reuse the existing `xi2_masks` selection storage. On KMS, a new input-thread command resyncs the libinput accumulator after a clamp.

**Tech Stack:** Rust; crates `yserver-protocol` (wire), `yserver-core` (dispatch/fanout), `yserver` (KMS/ynest backends). Reference: Xorg `Xi/xibarriers.c`, `mi/mipointer.c`, `Xi/exevents.c` at `/home/jos/Projects/xserver`; headers in `/usr/include/X11/extensions/`.

**Design spec:** `docs/superpowers/specs/2026-06-17-xfixes-pointer-barriers-design.md` (read it; it has the full wire layouts and Xorg citations).

**Conventions:**
- Format with `cargo +nightly fmt`; lint with plain `cargo clippy` (NOT pedantic); `cargo test`.
- XFIXES parse helpers return `Option`; the dispatcher ignores `None` (malformed).
- Error codes (`yserver_protocol::x11::error`): `BAD_VALUE=2`, `BAD_WINDOW=3`, `BAD_ACCESS=10`, `BAD_ALLOC=11`, `BAD_ID_CHOICE=14`, `BAD_LENGTH=16`. BadDevice = `XI1_ERROR_BAD_DEVICE` (= `crate::nested::XI2_FIRST_ERROR`).
- `XFIXES_MAJOR_OPCODE = 140`, `XI2_MAJOR_OPCODE = 137` (consts already in `process_request.rs`).
- Direction bits: `BarrierPositiveX=1`, `BarrierPositiveY=2`, `BarrierNegativeX=4`, `BarrierNegativeY=8`.

---

## File Structure

| File | Responsibility |
|------|----------------|
| `crates/yserver-protocol/src/x11/xfixes.rs` | minor consts 31/32; `parse_create_pointer_barrier`, `parse_delete_pointer_barrier` |
| `crates/yserver-protocol/src/x11/mod.rs` | `write_xi_barrier_event` encoder; `parse_xi_barrier_release` |
| `crates/yserver-core/src/server.rs` | `PointerBarrier` struct + `pointer_barriers` field; XID-namespace registration + test |
| `crates/yserver-core/src/core_loop/barriers.rs` (new) | pure clamp/direction/segment/hit-box functions + unit tests |
| `crates/yserver-core/src/core_loop/mod.rs` | `mod barriers;` |
| `crates/yserver-core/src/core_loop/process_request.rs` | XFIXES 31/32 arms; XI2 61 arm; WarpPointer bypass flag |
| `crates/yserver-core/src/core_loop/pointer_fanout.rs` | constrain hook + event emission |
| `crates/yserver-core/src/core_loop/message.rs` | `relative` bit on `HostInputEvent::PointerMotion` (T7) |
| `crates/yserver/src/input_thread.rs` + `kms/v2/backend.rs` | set `relative`; `process_pointer_absolute` → `barrier_bypass` for absolute/warp motion (T7) |
| `crates/yserver-core/src/core_loop/process_disconnect.rs` | owner sweep |
| `crates/yserver/src/input_thread.rs` (+ command enum) | `SetPosition` resync (Phase 4) |

## Phases

- **Phase 1** — Resource model + wire parse + create/delete/release dispatch + validation. No motion effect yet; barriers store/free correctly and all error paths work.
- **Phase 2** — Confinement clamp (pure module + fanout hook). The wall holds (ynest + core).
- **Phase 3** — XI2 `BarrierHit`/`BarrierLeave` events, `BarrierReleasePointer`, grab semantics, window-destroy drop.
- **Phase 4** — KMS input-thread `SetPosition` resync (HW-gated).

Commit after every task. Run `cargo +nightly fmt && cargo clippy -p <crate>` before each commit.

---

# Phase 1 — Resource model, wire parsing, dispatch, validation

### Task 1: PointerBarrier struct + `pointer_barriers` field

**Files:**
- Modify: `crates/yserver-core/src/server.rs` (struct near `XFixesRegion` ~line 1471; field near `xfixes_regions` line 883; init near line 1145)

- [ ] **Step 1: Add the struct.** Near the `XFixesRegion` definition (~line 1471) add:

```rust
/// XFIXES/XInput pointer barrier (XID resource, client-owned).
/// Mirrors Xorg `struct PointerBarrier` + `PointerBarrierClient` +
/// the single-master-pointer slice of `PointerBarrierDevice`
/// (`Xi/xibarriers.c`). yserver models one master pointer (device 2),
/// so per-device hit state collapses to these scalar fields.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PointerBarrier {
    pub owner: ClientId,
    /// `window` arg: selects screen, echoed as events' `event` window.
    pub window: ResourceId,
    pub x1: i16,
    pub y1: i16,
    pub x2: i16,
    pub y2: i16,
    /// Permitted-direction bitmask (& 0x0f, axis-irrelevant bits stripped).
    pub directions: u32,
    /// Device-id list (empty = all; wildcards 0/1 also = all).
    pub devices: Vec<u16>,
    // runtime hit-state for the master pointer:
    pub hit: bool,
    pub seen: bool,
    pub event_id: u32,
    pub release_event_id: u32,
    pub last_timestamp: u32,
}
```

- [ ] **Step 2: Add the field.** After the `xfixes_regions` field (line 883):

```rust
    pub pointer_barriers: HashMap<u32, PointerBarrier>,
```

- [ ] **Step 3: Initialize it.** In `ServerState::new` (and every other struct-literal initializer flagged by the compiler — there are several; the compiler will list them), beside `xfixes_regions: HashMap::new(),` add:

```rust
            pointer_barriers: HashMap::new(),
```

- [ ] **Step 4: Build.** Run: `cargo build -p yserver-core --locked` — fix every "missing field `pointer_barriers`" error by adding the init line at that site. Expected: clean build.

- [ ] **Step 5: Commit.**

```bash
git add crates/yserver-core/src/server.rs
git commit -m "feat(barriers): add PointerBarrier resource + pointer_barriers field"
```

### Task 2: Register the XID namespace (XC-MISC guard)

**Files:**
- Modify: `crates/yserver-core/src/server.rs` (`xid_occupied` ~1429, `used_xids_in` ~1444, test `xid_occupied_covers_every_namespace` ~4832)

- [ ] **Step 1: Extend the test first (it should fail).** In `xid_occupied_covers_every_namespace`, after the last extension-map insertion (the `present_event_selections` block), add a barrier and push its id to `expect`:

```rust
        // extension namespace: pointer barrier
        let id_barrier = base + 30;
        state.pointer_barriers.insert(
            id_barrier,
            crate::server::PointerBarrier {
                owner,
                window: ROOT_WINDOW,
                x1: 0, y1: 0, x2: 0, y2: 10,
                directions: 0,
                devices: Vec::new(),
                hit: false, seen: false,
                event_id: 1, release_event_id: 0, last_timestamp: 0,
            },
        );
        expect.push(id_barrier);
```

(Use an offset not already taken in the test; check the existing `base + N` values and pick a free one — `+ 30` shown as an example.)

- [ ] **Step 2: Run the test, verify it fails.** Run: `cargo test -p yserver-core xid_occupied_covers_every_namespace`
Expected: FAIL — `xid_occupied` returns false for the barrier id (assert mismatch).

- [ ] **Step 3: Register in `xid_occupied`.** Add to the `||` chain (after `present_event_selections`):

```rust
            || self.pointer_barriers.contains_key(&id)
```

- [ ] **Step 4: Register in `used_xids_in`.** Add (after the `present_event_selections` line):

```rust
        out.extend(self.pointer_barriers.keys().filter(in_range));
```

- [ ] **Step 5: Run the test, verify it passes.** Run: `cargo test -p yserver-core xid_occupied_covers_every_namespace`
Expected: PASS.

- [ ] **Step 6: Commit.**

```bash
git add crates/yserver-core/src/server.rs
git commit -m "feat(barriers): register pointer_barriers in XID namespace (XC-MISC guard)"
```

### Task 3: Parse CreatePointerBarrier / DeletePointerBarrier

**Files:**
- Modify: `crates/yserver-protocol/src/x11/xfixes.rs` (consts near line 34; parse fns near the other `parse_*`; tests in the `#[cfg(test)]` module)

- [ ] **Step 1: Write the failing tests.** In the `xfixes.rs` test module add:

```rust
    #[test]
    fn parse_create_pointer_barrier_basic() {
        // body (after generic header): barrier@0, window@4, x1@8,y1@10,
        // x2@12,y2@14, directions@16, pad@20, num_devices@22, devices@24.
        let mut body = Vec::new();
        body.extend_from_slice(&0x00aa_bbccu32.to_le_bytes()); // barrier
        body.extend_from_slice(&0x0000_0001u32.to_le_bytes()); // window (root)
        body.extend_from_slice(&100i16.to_le_bytes()); // x1
        body.extend_from_slice(&0i16.to_le_bytes());   // y1
        body.extend_from_slice(&100i16.to_le_bytes()); // x2 (vertical)
        body.extend_from_slice(&200i16.to_le_bytes()); // y2
        body.extend_from_slice(&0x0000_00FFu32.to_le_bytes()); // directions (only low 4 kept)
        body.extend_from_slice(&0u16.to_le_bytes());   // pad
        body.extend_from_slice(&2u16.to_le_bytes());   // num_devices
        body.extend_from_slice(&2u16.to_le_bytes());   // device 2
        body.extend_from_slice(&0u16.to_le_bytes());   // device 0 (XIAllDevices)

        let b = parse_create_pointer_barrier(&body).expect("parse");
        assert_eq!(b.barrier, 0x00aa_bbcc);
        assert_eq!(b.window, 1);
        assert_eq!((b.x1, b.y1, b.x2, b.y2), (100, 0, 100, 200));
        assert_eq!(b.directions, 0x0f, "only low 4 bits kept");
        assert_eq!(b.devices, vec![2u16, 0]);
    }

    #[test]
    fn parse_create_pointer_barrier_truncated_is_none() {
        assert!(parse_create_pointer_barrier(&[0u8; 10]).is_none());
        // num_devices says 3 but body has only 1 device worth of bytes
        let mut body = vec![0u8; 24];
        body[22] = 3; // num_devices = 3
        assert!(parse_create_pointer_barrier(&body).is_none());
    }

    #[test]
    fn parse_delete_pointer_barrier_reads_xid() {
        let body = 0xdead_beefu32.to_le_bytes();
        assert_eq!(parse_delete_pointer_barrier(&body), Some(0xdead_beef));
        assert_eq!(parse_delete_pointer_barrier(&[0u8; 2]), None);
    }
```

- [ ] **Step 2: Run, verify fail.** Run: `cargo test -p yserver-protocol parse_create_pointer_barrier`
Expected: FAIL — `parse_create_pointer_barrier` not found.

- [ ] **Step 3: Add consts + struct + parsers.** After `SHOW_CURSOR` const (line 34):

```rust
pub const CREATE_POINTER_BARRIER: u8 = 31;
pub const DELETE_POINTER_BARRIER: u8 = 32;
```

Add a request struct near the other request structs:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreatePointerBarrierRequest {
    pub barrier: u32,
    pub window: u32,
    pub x1: i16,
    pub y1: i16,
    pub x2: i16,
    pub y2: i16,
    pub directions: u32,
    pub devices: Vec<u16>,
}
```

Add parsers near the other `parse_*` fns:

```rust
#[must_use]
pub fn parse_create_pointer_barrier(body: &[u8]) -> Option<CreatePointerBarrierRequest> {
    if body.len() < 24 {
        return None;
    }
    let num_devices = read_u16_le(&body[22..]) as usize;
    let need = 24 + num_devices * 2;
    if body.len() < need {
        return None;
    }
    let devices = (0..num_devices)
        .map(|i| read_u16_le(&body[24 + i * 2..]))
        .collect();
    Some(CreatePointerBarrierRequest {
        barrier: read_u32_le(body),
        window: read_u32_le(&body[4..]),
        x1: read_i16_le(&body[8..]),
        y1: read_i16_le(&body[10..]),
        x2: read_i16_le(&body[12..]),
        y2: read_i16_le(&body[14..]),
        directions: read_u32_le(&body[16..]) & 0x0f,
        devices,
    })
}

#[must_use]
pub fn parse_delete_pointer_barrier(body: &[u8]) -> Option<u32> {
    if body.len() < 4 {
        return None;
    }
    Some(read_u32_le(body))
}
```

- [ ] **Step 4: Run, verify pass.** Run: `cargo test -p yserver-protocol parse_create_pointer_barrier parse_delete_pointer_barrier`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit.**

```bash
git add crates/yserver-protocol/src/x11/xfixes.rs
git commit -m "feat(barriers): parse CreatePointerBarrier/DeletePointerBarrier"
```

### Task 4: Dispatch Create/Delete with validation

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` (XFIXES `match minor` ~3847; add arms beside `CREATE_REGION` ~3968)
- Test: same file's test module (pattern: `handle_xfixes_request` driven like existing XFIXES tests)

Helper context: `x11xfixes` is the import alias for `yserver_protocol::x11::xfixes`. Use `state.xid_occupied(id)` (covers all namespaces) for the in-use check and `xid_out_of_client_range(state, client_id, id)` for the range check, mirroring `CREATE_REGION`. Validation order matches Xorg `XICreatePointerBarrier`: geometry → negative-on-own-axis → window → devices.

- [ ] **Step 1: Write the failing integration tests.** In the `process_request.rs` test module, add (adapt helpers `install_client`/`read_all_available` already used by other tests; build the request body the same way Task 3's test does, then call `handle_xfixes_request`):

```rust
    fn xfixes_create_barrier(
        state: &mut ServerState,
        client: ClientId,
        barrier: u32, window: u32,
        x1: i16, y1: i16, x2: i16, y2: i16,
        directions: u32, devices: &[u16],
    ) -> io::Result<RequestOutcome> {
        let mut body = Vec::new();
        body.extend_from_slice(&barrier.to_le_bytes());
        body.extend_from_slice(&window.to_le_bytes());
        body.extend_from_slice(&x1.to_le_bytes());
        body.extend_from_slice(&y1.to_le_bytes());
        body.extend_from_slice(&x2.to_le_bytes());
        body.extend_from_slice(&y2.to_le_bytes());
        body.extend_from_slice(&directions.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes()); // pad
        body.extend_from_slice(&(devices.len() as u16).to_le_bytes());
        for d in devices { body.extend_from_slice(&d.to_le_bytes()); }
        let header = RequestHeader { opcode: 140, data: 31, length_units: (8 + body.len() / 4) as u16 };
        handle_xfixes_request(state, &mut RecordingBackend::new(), None, client, SequenceNumber(1), header, &body)
    }

    #[test]
    fn create_pointer_barrier_stores_resource() {
        let mut state = ServerState::new();
        let _peer = install_client(&mut state, 1);
        let bid = 0x0040_0001u32; // any non-zero xid (install_client range is 0/u32::MAX)
        xfixes_create_barrier(&mut state, ClientId(1), bid, ROOT_WINDOW.0, 100, 0, 100, 200, 0, &[]).unwrap();
        let b = state.pointer_barriers.get(&bid).expect("stored");
        assert_eq!((b.x1, b.y1, b.x2, b.y2), (100, 0, 100, 200));
        assert_eq!(b.event_id, 1, "event_id starts at 1");
        assert_eq!(b.release_event_id, 0);
    }

    #[test]
    fn create_pointer_barrier_diagonal_is_bad_value() {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let bid = 0x0040_0001u32;
        xfixes_create_barrier(&mut state, ClientId(1), bid, ROOT_WINDOW.0, 0, 0, 50, 80, 0, &[]).unwrap();
        let bytes = read_all_available(&mut peer);
        assert_eq!(bytes[0], 0, "error");
        assert_eq!(bytes[1], x11::error::BAD_VALUE);
        assert!(state.pointer_barriers.get(&bid).is_none(), "not stored");
    }

    #[test]
    fn create_pointer_barrier_bad_window() {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let bid = 0x0040_0001u32;
        xfixes_create_barrier(&mut state, ClientId(1), bid, 0x9999, 100, 0, 100, 200, 0, &[]).unwrap();
        let bytes = read_all_available(&mut peer);
        assert_eq!(bytes[1], x11::error::BAD_WINDOW);
    }

    #[test]
    fn create_pointer_barrier_negative_on_fixed_axis_bad_value() {
        // vertical barrier (x1==x2) with negative x → BadValue.
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let bid = 0x0040_0001u32;
        xfixes_create_barrier(&mut state, ClientId(1), bid, ROOT_WINDOW.0, -1, 0, -1, 200, 0, &[]).unwrap();
        let bytes = read_all_available(&mut peer);
        assert_eq!(bytes[1], x11::error::BAD_VALUE);
    }

    #[test]
    fn create_pointer_barrier_bad_device() {
        // device 3 = keyboard, not a master pointer → BadDevice.
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let bid = 0x0040_0001u32;
        xfixes_create_barrier(&mut state, ClientId(1), bid, ROOT_WINDOW.0, 100, 0, 100, 200, 0, &[3]).unwrap();
        let bytes = read_all_available(&mut peer);
        assert_eq!(bytes[1], XI1_ERROR_BAD_DEVICE);
    }

    #[test]
    fn delete_pointer_barrier_frees_and_owner_checks() {
        let mut state = ServerState::new();
        let mut peer1 = install_client(&mut state, 1);
        let _peer2 = install_client(&mut state, 2);
        let bid = 0x0040_0001u32;
        xfixes_create_barrier(&mut state, ClientId(1), bid, ROOT_WINDOW.0, 100, 0, 100, 200, 0, &[]).unwrap();
        let _ = read_all_available(&mut peer1);
        // wrong client → BadAccess, not freed
        let dbody = bid.to_le_bytes();
        let dhdr = RequestHeader { opcode: 140, data: 32, length_units: 3 };
        handle_xfixes_request(&mut state, &mut RecordingBackend::new(), None, ClientId(2), SequenceNumber(2), dhdr, &dbody).unwrap();
        assert!(state.pointer_barriers.contains_key(&bid), "still present");
        // owner → freed
        handle_xfixes_request(&mut state, &mut RecordingBackend::new(), None, ClientId(1), SequenceNumber(3), dhdr, &dbody).unwrap();
        assert!(!state.pointer_barriers.contains_key(&bid));
    }
```

> Test-XID note: `install_client` sets `resource_id_base: 0, resource_id_mask: u32::MAX`, so any non-zero, currently-unused XID is in range — hence the `0x0040_0001` literal. No helper needed.

- [ ] **Step 2: Run, verify fail.** Run: `cargo test -p yserver-core create_pointer_barrier`
Expected: FAIL — barriers not dispatched (no error emitted / not stored).

- [ ] **Step 3: Add the dispatch arms.** In `handle_xfixes_request`'s `match minor`, beside `CREATE_REGION` (~line 3968) add:

```rust
        x11xfixes::CREATE_POINTER_BARRIER => {
            if let Some(req) = x11xfixes::parse_create_pointer_barrier(body) {
                // Xorg XICreatePointerBarrier validation order:
                // geometry → negative-on-own-axis → window → devices → xid.
                let horizontal = req.y1 == req.y2;
                let vertical = req.x1 == req.x2;
                if horizontal == vertical {
                    // diagonal (neither) or zero-length point (both) → BadValue
                    return emit_x11_error(state, client_id, sequence, x11::error::BAD_VALUE, req.barrier, XFIXES_MAJOR_OPCODE);
                }
                if (horizontal && (req.y1 < 0 || req.y2 < 0))
                    || (vertical && (req.x1 < 0 || req.x2 < 0))
                {
                    return emit_x11_error(state, client_id, sequence, x11::error::BAD_VALUE, req.barrier, XFIXES_MAJOR_OPCODE);
                }
                if state.resources.window(ResourceId(req.window)).is_none() {
                    return emit_x11_error(state, client_id, sequence, x11::error::BAD_WINDOW, req.window, XFIXES_MAJOR_OPCODE);
                }
                // device list: accept wildcards 0/1 and the master pointer (2);
                // anything else → BadDevice (xibarriers.c:301 + create-time IsMaster).
                for &d in &req.devices {
                    if !(d == 0 || d == 1 || d == 2) {
                        return emit_x11_error_with_minor(
                            state, client_id, sequence,
                            XI1_ERROR_BAD_DEVICE, u32::from(d), u16::from(x11xfixes::CREATE_POINTER_BARRIER), XFIXES_MAJOR_OPCODE,
                        );
                    }
                }
                // In-use XID → BadAlloc, matching Xorg XICreatePointerBarrier's
                // AddResource failure (xibarriers.c:827) + the spec's errors table.
                if state.xid_occupied(req.barrier) {
                    return emit_x11_error(state, client_id, sequence, x11::error::BAD_ALLOC, req.barrier, XFIXES_MAJOR_OPCODE);
                }
                // Out-of-client-range XID → BadIdChoice. Xorg enforces the
                // client owns the XID range inside its resource DB; yserver
                // does it explicitly here, uniformly with every other
                // resource-create (e.g. CreateColormap, CREATE_REGION). This
                // is intentional yserver behavior, not a divergence — a
                // client may only name XIDs in its assigned range.
                if xid_out_of_client_range(state, client_id, req.barrier) {
                    return emit_x11_error(state, client_id, sequence, x11::error::BAD_ID_CHOICE, req.barrier, XFIXES_MAJOR_OPCODE);
                }
                // Normalize x1<=x2, y1<=y2 (Xorg sort_min_max) — but skip if any
                // endpoint negative (ray convention). Devices are pre-validated.
                let (mut x1, mut x2) = (req.x1, req.x2);
                let (mut y1, mut y2) = (req.y1, req.y2);
                if x1 >= 0 && x2 >= 0 && x1 > x2 { std::mem::swap(&mut x1, &mut x2); }
                if y1 >= 0 && y2 >= 0 && y1 > y2 { std::mem::swap(&mut y1, &mut y2); }
                // Strip axis-irrelevant direction bits.
                let directions = if horizontal {
                    req.directions & !(1 | 4) // clear PositiveX|NegativeX
                } else {
                    req.directions & !(2 | 8) // clear PositiveY|NegativeY
                };
                state.pointer_barriers.insert(req.barrier, crate::server::PointerBarrier {
                    owner: client_id,
                    window: ResourceId(req.window),
                    x1, y1, x2, y2,
                    directions,
                    devices: req.devices,
                    hit: false, seen: false,
                    event_id: 1, release_event_id: 0, last_timestamp: 0,
                });
            }
        }
        x11xfixes::DELETE_POINTER_BARRIER => {
            if let Some(bid) = x11xfixes::parse_delete_pointer_barrier(body) {
                match state.pointer_barriers.get(&bid) {
                    None => {
                        return emit_x11_error(state, client_id, sequence, x11::error::BAD_VALUE, bid, XFIXES_MAJOR_OPCODE);
                    }
                    Some(b) if b.owner != client_id => {
                        return emit_x11_error(state, client_id, sequence, x11::error::BAD_ACCESS, bid, XFIXES_MAJOR_OPCODE);
                    }
                    Some(_) => {
                        // (Phase 3 will synthesize a BarrierLeave here if hit.)
                        state.pointer_barriers.remove(&bid);
                    }
                }
            }
        }
```

- [ ] **Step 4: Run, verify pass.** Run: `cargo test -p yserver-core create_pointer_barrier delete_pointer_barrier`
Expected: PASS.

- [ ] **Step 5: Commit.**

```bash
git add crates/yserver-core/src/core_loop/process_request.rs
git commit -m "feat(barriers): dispatch CreatePointerBarrier/DeletePointerBarrier with validation"
```

### Task 5: Free barriers on client disconnect

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_disconnect.rs` (~line 183, beside `xfixes_regions.retain`)
- Test: `process_request.rs` (or wherever disconnect tests live; reuse `delete_pointer_barrier_frees_and_owner_checks`'s setup)

- [ ] **Step 1: Write the failing test.** Add to the disconnect test module (find where other `xfixes_regions`-on-disconnect behavior is tested; mirror it):

```rust
    #[test]
    fn disconnect_frees_pointer_barriers() {
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        let _peer = install_client(&mut state, 1);
        let bid = 0x0040_0001u32; // any non-zero xid; install_client range is 0/u32::MAX
        state.pointer_barriers.insert(bid, crate::server::PointerBarrier {
            owner: ClientId(1), window: ROOT_WINDOW, x1: 0, y1: 0, x2: 0, y2: 10,
            directions: 0, devices: Vec::new(), hit: false, seen: false,
            event_id: 1, release_event_id: 0, last_timestamp: 0,
        });
        process_disconnect(&mut state, &mut backend, ClientId(1));
        assert!(state.pointer_barriers.is_empty());
    }
```

> Real signature: `process_disconnect(state: &mut ServerState, backend: &mut dyn Backend, client_id: ClientId)` (`process_disconnect.rs:81`).
> NOTE: emitting a released `BarrierLeave` when a *hit* barrier is freed at disconnect (Xorg `BarrierFreeBarrier`) is added in **Task 11** (Phase 3), once the event encoder exists. Phase 1 only frees the resource — correct, because no barrier can be `hit` until Phase 2/3 wire up the clamp + hit-state.

- [ ] **Step 2: Run, verify fail.** Run: `cargo test -p yserver-core disconnect_frees_pointer_barriers`
Expected: FAIL — barrier survives disconnect.

- [ ] **Step 3: Add the sweep.** Beside the `xfixes_regions.retain(...)` (~line 183):

```rust
    state
        .pointer_barriers
        .retain(|_, b| b.owner != client_id);
```

- [ ] **Step 4: Run, verify pass.** Run: `cargo test -p yserver-core disconnect_frees_pointer_barriers`
Expected: PASS.

- [ ] **Step 5: fmt + clippy + commit.**

```bash
cargo +nightly fmt
cargo clippy -p yserver-core -p yserver-protocol
git add -A
git commit -m "feat(barriers): free pointer barriers on client disconnect"
```

---

# Phase 2 — Confinement clamp

### Task 6: Pure clamp module `barriers.rs`

**Files:**
- Create: `crates/yserver-core/src/core_loop/barriers.rs`
- Modify: `crates/yserver-core/src/core_loop/mod.rs` (add `mod barriers;` — check whether siblings are `pub mod` or `mod` and match)

Direction constants and the math are a direct port of `Xi/xibarriers.c`. Operate on `i32` coords (root space).

- [ ] **Step 1: Write the module with tests.** Create `barriers.rs`:

```rust
//! Pure pointer-barrier geometry — a port of Xorg `Xi/xibarriers.c`'s
//! clamp/segment/direction logic. No `ServerState`; unit-testable.
//! Coordinates are i32 root-space.

pub const POSITIVE_X: u32 = 1;
pub const POSITIVE_Y: u32 = 2;
pub const NEGATIVE_X: u32 = 4;
pub const NEGATIVE_Y: u32 = 8;

/// Minimal geometry view of a barrier for the clamp math.
#[derive(Clone, Copy, Debug)]
pub struct BarrierGeom {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
    pub directions: u32,
}

impl BarrierGeom {
    fn is_vertical(&self) -> bool { self.x1 == self.x2 }
    fn is_horizontal(&self) -> bool { self.y1 == self.y2 }
}

/// Direction bits of travel from (x1,y1)→(x2,y2).
#[must_use]
pub fn direction_of(x1: i32, y1: i32, x2: i32, y2: i32) -> u32 {
    let mut d = 0;
    if x2 > x1 { d |= POSITIVE_X; } else if x2 < x1 { d |= NEGATIVE_X; }
    if y2 > y1 { d |= POSITIVE_Y; } else if y2 < y1 { d |= NEGATIVE_Y; }
    d
}

/// A direction is blocked unless its allow-bit is set.
#[must_use]
fn blocks_direction(directions: u32, dir: u32) -> bool {
    (directions & dir) != dir
}

/// Xorg `inside_segment`: negative endpoints encode rays / infinite lines.
#[must_use]
fn inside_segment(v: i32, v1: i32, v2: i32) -> bool {
    if v1 < 0 && v2 < 0 { true }
    else if v1 < 0 { v <= v2 }
    else if v2 < 0 { v >= v1 }
    else { v1 <= v && v <= v2 }
}

/// Does the move (x1,y1)→(x2,y2) geometrically cross `b`? Returns the
/// distance from the move origin to the crossing if so. Port of
/// `barrier_is_blocking` (xibarriers.c). Macros: T=(v-a)/(b-a),
/// F(t,a,b)=t*(a-b)+a.
#[must_use]
pub fn is_blocking(b: &BarrierGeom, x1: i32, y1: i32, x2: i32, y2: i32) -> Option<f64> {
    let (x1, y1, x2, y2) = (x1 as f64, y1 as f64, x2 as f64, y2 as f64);
    if b.is_vertical() {
        let bx = f64::from(b.x1);
        if (x2 - x1).abs() < f64::EPSILON { return None; }
        let t = (bx - x1) / (x2 - x1);
        if !(0.0..=1.0).contains(&t) { return None; }
        if x2 > x1 && t == 0.0 { return None; } // sitting on barrier, moving +X away
        let y = t * (y1 - y2) + y1;
        // Xorg passes the float `y` to `inside_segment(int v, ...)`,
        // relying on C float→int TRUNCATION toward zero. `as i32`
        // matches; do NOT round (changes fractional-crossing hits).
        #[allow(clippy::cast_possible_truncation)]
        if !inside_segment(y as i32, b.y1, b.y2) { return None; }
        Some(((y - y1).powi(2) + (bx - x1).powi(2)).sqrt())
    } else {
        // horizontal: mirror image
        let by = f64::from(b.y1);
        if (y2 - y1).abs() < f64::EPSILON { return None; }
        let t = (by - y1) / (y2 - y1);
        if !(0.0..=1.0).contains(&t) { return None; }
        if y2 > y1 && t == 0.0 { return None; }
        let x = t * (x1 - x2) + x1;
        #[allow(clippy::cast_possible_truncation)]
        if !inside_segment(x as i32, b.x1, b.x2) { return None; } // truncate, not round (Xorg float→int)
        Some(((x - x1).powi(2) + (by - y1).powi(2)).sqrt())
    }
}

/// Clamp (x,y) to barrier `b` given travel `dir`. Port of
/// `barrier_clamp_to_barrier`. Only the blocking axis is modified.
pub fn clamp_to_barrier(b: &BarrierGeom, dir: u32, x: &mut i32, y: &mut i32) {
    if b.is_vertical() {
        if (dir & NEGATIVE_X) & !b.directions != 0 { *x = b.x1; }
        if (dir & POSITIVE_X) & !b.directions != 0 { *x = b.x1 - 1; }
    }
    if b.is_horizontal() {
        if (dir & NEGATIVE_Y) & !b.directions != 0 { *y = b.y1; }
        if (dir & POSITIVE_Y) & !b.directions != 0 { *y = b.y1 - 1; }
    }
}

/// Among `candidates` (index, geom), the nearest that (a) is not in
/// `seen`, (b) blocks a bit of `dir`, (c) geometrically blocks the move.
/// Returns (index, distance, geom). Port of `barrier_find_nearest`.
#[must_use]
pub fn find_nearest<'a>(
    candidates: &'a [(usize, BarrierGeom)],
    seen: &[usize],
    dir: u32,
    x1: i32, y1: i32, x2: i32, y2: i32,
) -> Option<(usize, f64, BarrierGeom)> {
    let mut best: Option<(usize, f64, BarrierGeom)> = None;
    for &(idx, b) in candidates {
        if seen.contains(&idx) { continue; }
        // must block at least one travelled direction
        let dir_blocked = (b.directions != 0 && {
            let mut any = false;
            for d in [POSITIVE_X, POSITIVE_Y, NEGATIVE_X, NEGATIVE_Y] {
                if dir & d != 0 && blocks_direction(b.directions, d) { any = true; }
            }
            any
        }) || b.directions == 0;
        if !dir_blocked { continue; }
        if let Some(dist) = is_blocking(&b, x1, y1, x2, y2) {
            if best.map_or(true, |(_, bd, _)| dist < bd) {
                best = Some((idx, dist, b));
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vbar(x: i32, y1: i32, y2: i32, dirs: u32) -> BarrierGeom {
        BarrierGeom { x1: x, y1, x2: x, y2, directions: dirs }
    }

    // Solid vertical barrier at x=100, y in [0,200].
    // Approaching from -X side (90→110) clamps to x1-1 = 99.
    #[test]
    fn solid_vertical_from_left_clamps_to_x1_minus_1() {
        let b = vbar(100, 0, 200, 0);
        assert!(is_blocking(&b, 90, 50, 110, 50).is_some());
        let (mut x, mut y) = (110, 50);
        clamp_to_barrier(&b, direction_of(90, 50, 110, 50), &mut x, &mut y);
        assert_eq!((x, y), (99, 50));
    }

    // Approaching from +X side (110→90) clamps to x1 = 100.
    #[test]
    fn solid_vertical_from_right_clamps_to_x1() {
        let b = vbar(100, 0, 200, 0);
        let (mut x, mut y) = (90, 50);
        clamp_to_barrier(&b, direction_of(110, 50, 90, 50), &mut x, &mut y);
        assert_eq!((x, y), (100, 50));
    }

    // Barrier PERMITS NegativeX: a right→left move is not clamped.
    #[test]
    fn permitted_direction_passes_through() {
        let b = vbar(100, 0, 200, NEGATIVE_X);
        let (mut x, mut y) = (90, 50);
        clamp_to_barrier(&b, direction_of(110, 50, 90, 50), &mut x, &mut y);
        assert_eq!((x, y), (90, 50), "permitted NegativeX not clamped");
    }

    // Move that misses the segment span (y outside [0,200]) does not block.
    #[test]
    fn miss_outside_segment() {
        let b = vbar(100, 0, 200, 0);
        assert!(is_blocking(&b, 90, 300, 110, 300).is_none());
    }

    // Sitting exactly on the barrier moving +X away → not blocking.
    #[test]
    fn on_barrier_moving_away_not_blocking() {
        let b = vbar(100, 0, 200, 0);
        assert!(is_blocking(&b, 100, 50, 110, 50).is_none());
    }

    // Ray: y2 negative → inside iff y >= y1.
    #[test]
    fn inside_segment_ray_semantics() {
        assert!(inside_segment(500, 0, -1));   // ray from 0 upward
        assert!(!inside_segment(-5, 0, -1));
        assert!(inside_segment(123, -1, -1));  // infinite line
    }
}
```

- [ ] **Step 2: Register the module.** In `crates/yserver-core/src/core_loop/mod.rs` add (matching sibling visibility):

```rust
mod barriers;
```

- [ ] **Step 3: Run, verify pass.** Run: `cargo test -p yserver-core core_loop::barriers`
Expected: PASS (7 tests).

- [ ] **Step 4: Commit.**

```bash
git add crates/yserver-core/src/core_loop/barriers.rs crates/yserver-core/src/core_loop/mod.rs
git commit -m "feat(barriers): pure clamp/segment/direction module (port of xibarriers.c)"
```

### Task 7: Hook the clamp into the motion fanout

**Files:**
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs` (insert after the confinement block, ~line 120, before `let now = ...`)
- Modify: `crates/yserver-core/src/server.rs` (add `pub barrier_bypass: bool,` field + init `false`)
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` (`handle_warp_pointer`: set/clear `barrier_bypass` around the warp)
- Test: `pointer_fanout.rs` test module

The clamp runs only for genuine relative motion: skip when `is_replay`, when `barrier_bypass` is set (WarpPointer / corrective warp), and when there are no barriers.

- [ ] **Step 1: Write the failing test.** In `pointer_fanout.rs` tests (mirror existing fanout tests that build a `HostPointerEvent` and call `pointer_event_fanout_to_state`):

```rust
    #[test]
    fn motion_clamps_against_solid_barrier() {
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        // solid vertical barrier at x=100, y in [0,200], owned by client 1
        state.pointer_barriers.insert(0x500001, crate::server::PointerBarrier {
            owner: ClientId(1), window: ROOT_WINDOW, x1: 100, y1: 0, x2: 100, y2: 200,
            directions: 0, devices: Vec::new(), hit: false, seen: false,
            event_id: 1, release_event_id: 0, last_timestamp: 0,
        });
        state.pointer_root = (90, 50); // currently left of the barrier
        // motion to (110,50): should clamp to (99,50)
        let mut ev = motion_event(); ev.root_x = 110; ev.root_y = 50;
        pointer_event_fanout_to_state(&mut state, &mut backend, &HostXidMap::new(), ev, true, false);
        assert_eq!(state.pointer_root, (99, 50), "clamped to x1-1");
    }

    #[test]
    fn barrier_bypass_skips_clamp() {
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        state.pointer_barriers.insert(0x500001, crate::server::PointerBarrier {
            owner: ClientId(1), window: ROOT_WINDOW, x1: 100, y1: 0, x2: 100, y2: 200,
            directions: 0, devices: Vec::new(), hit: false, seen: false,
            event_id: 1, release_event_id: 0, last_timestamp: 0,
        });
        state.pointer_root = (90, 50);
        state.barrier_bypass = true; // simulate WarpPointer
        let mut ev = motion_event(); ev.root_x = 110; ev.root_y = 50;
        pointer_event_fanout_to_state(&mut state, &mut backend, &HostXidMap::new(), ev, true, false);
        assert_eq!(state.pointer_root, (110, 50), "warp not clamped");
    }
```

> `motion_event()` (`pointer_fanout.rs:1937`) returns a `MotionNotify` `HostPointerEvent`; override `.root_x`/`.root_y` for the test. `HostXidMap::new()` (`host_x11/mod.rs:1383`) is the empty map. Both are already used by sibling fanout tests.

- [ ] **Step 2: Run, verify fail.** Run: `cargo test -p yserver-core motion_clamps_against_solid_barrier`
Expected: FAIL — no clamp; `pointer_root` is (110,50). Also a compile error for the missing `barrier_bypass` field — add it in Step 3.

- [ ] **Step 3a: Add the field.** In `server.rs` beside `confine_warp_active` (line 832): `pub barrier_bypass: bool,`; init `false` beside `confine_warp_active: false` (line 1134) and any other initializer the compiler flags.

- [ ] **Step 3b: Insert the clamp** in `pointer_fanout.rs` after the confinement block (after its closing `}` ~line 120), before `let now = ...`:

```rust
    // Pointer barriers (Xorg input_constrain_cursor): clamp genuine
    // RELATIVE device motion only (Xorg `mode == Relative`). `barrier_bypass`
    // is the "this motion is absolute/synthetic, don't clamp" signal:
    // Step 3c sets it in `process_pointer_absolute` for libinput
    // `MotionAbsolute` (touch/tablet) AND for warp-injected motion;
    // `confine_warp_active` covers the confine reclamp; the barrier's own
    // corrective warp sets `barrier_bypass` too. Relative mouse motion
    // leaves both clear → clamps. ynest motion (advisory) doesn't pass
    // through process_pointer_absolute, so it clamps the reported coords.
    // Replays are exempt.
    if !is_replay
        && !state.barrier_bypass
        && !state.confine_warp_active
        && !state.pointer_barriers.is_empty()
    {
        let (ox, oy) = (i32::from(state.pointer_root.0), i32::from(state.pointer_root.1));
        let mut nx = i32::from(event.root_x);
        let mut ny = i32::from(event.root_y);
        if (nx, ny) != (ox, oy) {
            use crate::core_loop::barriers::{BarrierGeom, direction_of, find_nearest, clamp_to_barrier};
            // Snapshot candidate geoms WITH their barrier XID key. `idx`
            // (the find_nearest index) maps back to `keys[idx]` so the
            // loop — and the Phase-3 release short-circuit (Task 10) —
            // can `state.pointer_barriers.get_mut(&keys[idx])`. Snapshot
            // up front so we don't borrow `state.pointer_barriers` across
            // the `backend.warp_pointer_root(state, …)` call below.
            let keys: Vec<u32> = state.pointer_barriers.keys().copied().collect();
            let candidates: Vec<(usize, BarrierGeom)> = keys
                .iter()
                .enumerate()
                .map(|(i, k)| {
                    let b = &state.pointer_barriers[k];
                    (i, BarrierGeom {
                        x1: i32::from(b.x1), y1: i32::from(b.y1),
                        x2: i32::from(b.x2), y2: i32::from(b.y2),
                        directions: b.directions,
                    })
                })
                .collect();
            let (mut cx, mut cy) = (ox, oy);
            let mut seen: Vec<usize> = Vec::new();
            let mut dir = direction_of(cx, cy, nx, ny);
            while dir != 0 {
                let Some((idx, _dist, geom)) = find_nearest(&candidates, &seen, dir, cx, cy, nx, ny) else { break; };
                seen.push(idx);
                let key = keys[idx];
                // Phase 3 / Task 10: the release short-circuit lives here —
                //   if let Some(b) = state.pointer_barriers.get(&key)
                //       && b.event_id == b.release_event_id { continue; }
                clamp_to_barrier(&geom, dir, &mut nx, &mut ny);
                // resolve one axis per pass
                if geom.x1 == geom.x2 { dir &= !(crate::core_loop::barriers::POSITIVE_X | crate::core_loop::barriers::NEGATIVE_X); cx = nx; }
                else { dir &= !(crate::core_loop::barriers::POSITIVE_Y | crate::core_loop::barriers::NEGATIVE_Y); cy = ny; }
                // (Phase 3 / Task 9: state.pointer_barriers.get_mut(&key) → mark hit + emit BarrierHit.)
            }
            #[allow(clippy::cast_possible_truncation)]
            {
                event.root_x = nx as i16;
                event.root_y = ny as i16;
            }
            if (nx, ny) != (i32::from(state.pointer_root.0), i32::from(state.pointer_root.1)) && !state.confine_warp_active {
                state.confine_warp_active = true;
                state.barrier_bypass = true;
                backend.warp_pointer_root(state, nx, ny);
                state.barrier_bypass = false;
                state.confine_warp_active = false;
            }
        }
    }
```

- [ ] **Step 3c: Tag motion source (relative vs absolute) — the real "Relative-only" gate.** Xorg constrains `mode == Relative` motion only. yserver collapses BOTH relative mouse motion (`InputEvent::PointerMotion{dx,dy}`) and absolute touch/tablet motion (`InputEvent::PointerMotionAbsolute`) into the same `HostInputEvent::PointerMotion{x,y,time}` at `input_thread.rs:132/141` — losing the distinction. There IS a live absolute path (libinput `MotionAbsolute` → `process_pointer_absolute` → fanout), so the clamp must NOT fire on it. Thread a `relative` bit and convert it to `barrier_bypass` at the KMS dispatch. This also covers warps for free (warp-injected motion is non-relative), so no separate per-warp-site helper is needed.

  1. **`crates/yserver-core/src/core_loop/message.rs:191`** — add a field to the `PointerMotion` variant: `PointerMotion { x: i32, y: i32, time: u32, relative: bool }`.
  2. **Set `relative` at EVERY construction site** (grep `HostInputEvent::PointerMotion` to confirm none missed — these are the complete set as of 2026-06-17):
     - `input_thread.rs:132` — `InputEvent::PointerMotion{dx,dy}` (relative mouse) → **`relative: true`**.
     - `input_thread.rs:141` — `InputEvent::PointerMotionAbsolute` (touch/tablet) → **`relative: false`**.
     - `kms/v2/backend.rs:15994` — the warp-injection `PointerMotion` fed by `warp_pointer_root` via `on_host_input` → **`relative: false`**.
     - `process_request.rs:6206` — the XTEST `FakeInput` synthetic motion (absolute) → **`relative: false`**.
     - `kms/v2/backend.rs:19896` — KMS unit-test constructor → **`relative: false`** (test).
  3. **Fix EVERY exact-match consumer that will stop compiling** (patterns without `..` break on the new field):
     - `kms/v2/backend.rs:9175` — `HostInputEvent::PointerMotion { x, y, time: _ }`. **This is the consumer that routes to `process_pointer_absolute`** — bind the bit: `{ x, y, time: _, relative }`, and forward `relative` into the `process_pointer_absolute(...)` call (add it as a parameter).
     - `host_x11/trait_impl.rs:89` — `{ x, y, time }` → add `, ..` (ynest path; doesn't need the bit).
     - `input_thread.rs:933` — test `{ x, y, time }` → add `, ..`.
     - Sites already using `..` (`input_thread.rs:468,811,860,1015,1027,1072`, `backend.rs:10372`) compile unchanged.
  4. **`crates/yserver/src/kms/v2/backend.rs` `process_pointer_absolute` (~:5597)** — gains the `relative: bool` parameter. Update ALL callers: the runtime forwarder at `:9176` passes the bound `relative`; the four direct unit-test callers (`:19663`, `:19667`, `:19697`, `:19706`) pass `true` (those tests assert FB-extent clamping, not barriers, so the value is immaterial — `true` keeps them on the normal path). Then wrap the `dispatch_motion_event(server_state)` call so non-relative motion bypasses the barrier clamp:
     ```rust
     let prev = server_state.barrier_bypass;
     server_state.barrier_bypass = prev || !relative;
     self.dispatch_motion_event(server_state);
     server_state.barrier_bypass = prev;
     ```
  This makes the Step-3b gate (`!barrier_bypass && !confine_warp_active`) fire for relative mouse motion only. Absolute touch/tablet, XTEST FakeInput, and every KMS warp (which re-injects `relative:false`) skip the clamp — matching Xorg. ynest motion does not pass through `process_pointer_absolute`, so it still clamps (the spec's documented "advisory on ynest" behavior — host owns the real sprite).

  > Why no `warp_root_no_barrier` helper: every server warp on KMS re-enters via `process_pointer_absolute` with `relative:false`, so it's already gated here. The core-level warp sites (WarpPointer `process_request.rs:22029/22043`, RANDR-shrink `run.rs:951`, `confine_pointer_now :22810`) need no change. The barrier's own corrective warp in Step 3b additionally sets `barrier_bypass` directly as a re-entrancy guard.

- [ ] **Step 3d: Add the absolute-not-clamped regression note.** A pure-core unit test can't exercise `process_pointer_absolute` (it's in the `yserver` KMS crate). Cover the gate two ways: (a) the `barrier_bypass_skips_clamp` core test above proves the gate; (b) add a `yserver`-crate test that drives `process_pointer_absolute` with `relative:false` while a barrier is active and asserts the position is NOT clamped (mirror an existing `process_pointer_absolute` test). If no such harness exists, record this as a HW-smoke check in Task 13 (touchscreen drag across a barrier must pass through).

- [ ] **Step 4: Run, verify pass.** Run: `cargo test -p yserver-core motion_clamps_against_solid_barrier barrier_bypass_skips_clamp` and `cargo build -p yserver` (the `relative` field touches the KMS crate).
Expected: PASS / clean build.

- [ ] **Step 5: fmt + clippy + full test + commit.**

```bash
cargo +nightly fmt
cargo clippy -p yserver-core
cargo test -p yserver-core -p yserver-protocol
git add -A
git commit -m "feat(barriers): clamp pointer motion against active barriers (confinement)"
```

---

# Phase 3 — XI2 events, release, grab semantics

### Task 8: Encode the xXIBarrierEvent

**Files:**
- Modify: `crates/yserver-protocol/src/x11/mod.rs` (encoder + test)

Wire layout (68 bytes, length field = 9): see spec "Events" table. `FP1616` = i32; `FP3232` = `{i32 integral; u32 frac}`.

- [ ] **Step 1: Write the failing test.**

```rust
    #[test]
    fn xi_barrier_event_layout() {
        // BarrierHit, device 2, barrier 0x55, window 0x10, root 0x01,
        // eventid 7, time 1234, root_x/y = 99/50, dx=20.0 dy=0.
        let mut buf = Vec::new();
        write_xi_barrier_event(
            &mut buf, ClientByteOrder::LittleEndian, SequenceNumber(3),
            /*xi_major*/137, /*evtype*/25, /*deviceid*/2, /*time*/1234,
            /*eventid*/7, /*root*/0x01, /*event_win*/0x10, /*barrier*/0x55,
            /*dtime*/0, /*flags*/0, /*sourceid*/2,
            /*root_x*/99, /*root_y*/50, /*dx*/20.0, /*dy*/0.0,
        );
        assert_eq!(buf.len(), 68);
        assert_eq!(buf[0], 35, "GenericEvent");
        assert_eq!(buf[1], 137, "extension");
        assert_eq!(&buf[4..8], &9u32.to_le_bytes(), "length = (68-32)/4 = 9");
        assert_eq!(&buf[8..10], &25u16.to_le_bytes(), "evtype Hit");
        assert_eq!(&buf[10..12], &2u16.to_le_bytes(), "deviceid");
        assert_eq!(&buf[16..20], &7u32.to_le_bytes(), "eventid");
        assert_eq!(&buf[28..32], &0x55u32.to_le_bytes(), "barrier");
        // root_x is FP1616: 99 << 16
        assert_eq!(&buf[44..48], &(99i32 << 16).to_le_bytes(), "root_x FP1616");
        // dx is FP3232: integral 20, frac 0
        assert_eq!(&buf[52..56], &20i32.to_le_bytes(), "dx integral");
        assert_eq!(&buf[56..60], &0u32.to_le_bytes(), "dx frac");
    }
```

- [ ] **Step 2: Run, verify fail.** Run: `cargo test -p yserver-protocol xi_barrier_event_layout`
Expected: FAIL — fn not found.

- [ ] **Step 3: Implement the encoder** in `mod.rs`:

```rust
/// XI2 `xXIBarrierEvent` (XI_BarrierHit=25 / XI_BarrierLeave=26),
/// XI2proto.h:1068. GenericEvent, 68 bytes, length=9. `dx`/`dy` are
/// `FP3232` (integral i32 + frac u32); `root_x`/`root_y` are FP1616.
#[allow(clippy::too_many_arguments)]
pub fn write_xi_barrier_event(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    xi_major: u8,
    evtype: u16,
    deviceid: u16,
    time: u32,
    eventid: u32,
    root: u32,
    event_window: u32,
    barrier: u32,
    dtime: u32,
    flags: u32,
    sourceid: u16,
    root_x: i32,
    root_y: i32,
    dx: f64,
    dy: f64,
) {
    fn fp1616(v: i32) -> i32 { v << 16 }
    // Port of Xorg dix/inpututils.c `double_to_fp3232`: integral = floor(in),
    // frac = (in - integral) * 2^32. floor (NOT trunc) so negative deltas
    // encode correctly: -1.5 → integral -2, frac 0.5*2^32.
    fn fp3232(v: f64) -> (i32, u32) {
        let integral_f = v.floor();
        #[allow(clippy::cast_possible_truncation)]
        let integral = integral_f as i32;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let frac = ((v - integral_f) * 4_294_967_296.0_f64) as u32; // 1<<32
        (integral, frac)
    }
    let mut out = Vec::with_capacity(68);
    out.push(35); // GenericEvent
    out.push(xi_major);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, 9); // length
    write_u16(byte_order, &mut out, evtype);
    write_u16(byte_order, &mut out, deviceid);
    write_u32(byte_order, &mut out, time);
    write_u32(byte_order, &mut out, eventid);
    write_u32(byte_order, &mut out, root);
    write_u32(byte_order, &mut out, event_window);
    write_u32(byte_order, &mut out, barrier);
    write_u32(byte_order, &mut out, dtime);
    write_u32(byte_order, &mut out, flags);
    write_u16(byte_order, &mut out, sourceid);
    write_u16(byte_order, &mut out, 0); // pad
    write_u32(byte_order, &mut out, fp1616(root_x) as u32);
    write_u32(byte_order, &mut out, fp1616(root_y) as u32);
    let (dxi, dxf) = fp3232(dx);
    let (dyi, dyf) = fp3232(dy);
    write_u32(byte_order, &mut out, dxi as u32);
    write_u32(byte_order, &mut out, dxf);
    write_u32(byte_order, &mut out, dyi as u32);
    write_u32(byte_order, &mut out, dyf);
    debug_assert_eq!(out.len(), 68);
    let _ = writer.write_all(&out);
}
```

- [ ] **Step 4: Run, verify pass.** Run: `cargo test -p yserver-protocol xi_barrier_event_layout`
Expected: PASS.

- [ ] **Step 5: Commit.**

```bash
git add crates/yserver-protocol/src/x11/mod.rs
git commit -m "feat(barriers): encode xXIBarrierEvent (Hit/Leave) wire layout"
```

### Task 9: Emit BarrierHit/Leave from the clamp; delivery + grab flag

**Files:**
- Modify: `crates/yserver-core/src/core_loop/pointer_fanout.rs` (the clamp loop from Task 7 + a post-loop leave sweep)

Delivery rule (Xorg `ProcessBarrierEvent`, `exevents.c:1724`): look up `barrier.window`; if gone, drop (no fanout). Set `XIBarrierDeviceIsGrabbed` (1<<1) when the pointer is grabbed. Grabbed-path delivery only when the barrier's owner holds the grab AND grab window == barrier window; else normal `xi2_masks[(barrier.window, 2)]` delivery (flag still set). Use the existing `xi2_mask_for_client` (`server.rs:2168`) + `fanout_event_to_clients` (`fanout.rs:126`) machinery — follow how an existing XI2 event (e.g. the property/mapping notify path) gathers targets and writes.

- [ ] **Step 1: Write the failing test.** Drive a motion into a solid barrier with a client that selected `XI_BarrierHit` on the root, assert it receives a 68-byte GenericEvent with evtype 25:

```rust
    #[test]
    fn barrier_hit_event_delivered_to_selecting_client() {
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        let mut peer = install_client(&mut state, 1);
        // client 1 selects XI_BarrierHit (bit 25) on the root for master pointer (2)
        if let Some(c) = state.clients.get_mut(&1) {
            c.xi2_masks.insert((ROOT_WINDOW, 2), 1 << 25);
        }
        let bid = 0x500001;
        state.pointer_barriers.insert(bid, crate::server::PointerBarrier {
            owner: ClientId(1), window: ROOT_WINDOW, x1: 100, y1: 0, x2: 100, y2: 200,
            directions: 0, devices: Vec::new(), hit: false, seen: false,
            event_id: 1, release_event_id: 0, last_timestamp: 0,
        });
        state.pointer_root = (90, 50);
        let mut ev = motion_event(); ev.root_x = 110; ev.root_y = 50;
        pointer_event_fanout_to_state(&mut state, &mut backend, &HostXidMap::new(), ev, true, false);
        let bytes = read_all_available(&mut peer);
        // find the GenericEvent (type 35, evtype 25) in the stream
        assert!(bytes.windows(2).any(|w| w[0] == 35), "GenericEvent present");
        let b = state.pointer_barriers.get(&bid).unwrap();
        assert!(b.hit, "barrier marked hit");
        assert_eq!(b.event_id, 1, "event_id unchanged while hit");
    }
```

- [ ] **Step 2: Run, verify fail.** Run: `cargo test -p yserver-core barrier_hit_event_delivered_to_selecting_client`
Expected: FAIL — no event emitted; `hit` false.

- [ ] **Step 3: Extend the clamp loop.** Replace the `// (Phase 3: ...)` comment in Task 7's loop with: set `new_sequence = !barrier.hit`; set `barrier.hit = true`; compute `dtime`; build the event via `write_xi_barrier_event` with `dx = (nx_proposed - ox) as f64`, `dy = (ny_proposed - oy) as f64`, `root_x/root_y` = final clamped; resolve grab (flag + routing); deliver. Then after the loop, add the leave sweep: for each barrier with `hit == true` whose final position is outside its hit box (`HIT_EDGE_EXTENTS = 2`), set `hit = false`, emit `BarrierLeave` (evtype 26), and `event_id += 1`.

> This step is larger than 5 minutes — split into 3 commits if needed: (a) mark hit + emit Hit (no grab special-casing, plain `xi2_masks` delivery); (b) add grab flag + grabbed-path routing; (c) add the leave sweep. Write a focused test for each before implementing. The detailed delivery code mirrors the existing XI2 event fanout — read `fanout_event_to_clients` + `xi2_mask_for_client` usage at an existing XI2 emit site and copy the target-gathering shape.

- [ ] **Step 4: Run, verify pass.** Run: `cargo test -p yserver-core barrier_hit barrier_leave`
Expected: PASS.

- [ ] **Step 5: fmt + clippy + commit.**

```bash
cargo +nightly fmt && cargo clippy -p yserver-core
git add -A && git commit -m "feat(barriers): emit BarrierHit/Leave with grab semantics + delivery"
```

### Task 10: Parse + dispatch XIBarrierReleasePointer; one-shot release

**Files:**
- Modify: `crates/yserver-protocol/src/x11/mod.rs` (`parse_xi_barrier_release`)
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` (XI2 minor 61 arm in `handle_xi2_request` ~9775; the release short-circuit in the clamp loop)

- [ ] **Step 1: Write the failing parse + state-machine tests.**

```rust
    // protocol crate:
    #[test]
    fn parse_xi_barrier_release_entries() {
        let mut body = Vec::new();
        body.extend_from_slice(&2u32.to_le_bytes()); // num_barriers
        for (dev, bar, eid) in [(2u16, 0x55u32, 7u32), (2, 0x66, 9)] {
            body.extend_from_slice(&dev.to_le_bytes());
            body.extend_from_slice(&0u16.to_le_bytes()); // pad
            body.extend_from_slice(&bar.to_le_bytes());
            body.extend_from_slice(&eid.to_le_bytes());
        }
        let v = parse_xi_barrier_release(&body).expect("parse");
        assert_eq!(v, vec![(2, 0x55, 7), (2, 0x66, 9)]);
    }
```

```rust
    // core crate (integration): release lets the pointer cross once, then re-arms.
    #[test]
    fn release_lets_pointer_cross_then_rearms() {
        // 1. hit a solid barrier (event_id stays 1, hit=true).
        // 2. XIBarrierReleasePointer{dev 2, barrier, eventid 1} → release_event_id=1.
        // 3. motion across the barrier now passes (clamp skipped).
        // 4. once outside the hit box, leave sweep bumps event_id to 2 (re-armed).
        // Build with the helpers from Tasks 7/9; assert pointer_root crosses in step 3.
    }
```

> Flesh out `release_lets_pointer_cross_then_rearms` using the same helpers; the assertions are: after release, a motion that would otherwise clamp produces `pointer_root` on the far side; after leaving the hit box, `event_id == 2`.

- [ ] **Step 2: Run, verify fail.** Run: `cargo test -p yserver-protocol parse_xi_barrier_release` and `cargo test -p yserver-core release_lets_pointer_cross`
Expected: FAIL.

- [ ] **Step 3a: Implement the parser** in `mod.rs`:

```rust
/// XI2 `XIBarrierReleasePointer` (minor 61). Returns
/// `(deviceid, barrier, eventid)` triples.
#[must_use]
pub fn parse_xi_barrier_release(body: &[u8]) -> Option<Vec<(u16, u32, u32)>> {
    if body.len() < 4 { return None; }
    let n = u32::from_le_bytes([body[0], body[1], body[2], body[3]]) as usize;
    if body.len() < 4 + n * 12 { return None; }
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let off = 4 + i * 12;
        let dev = u16::from_le_bytes([body[off], body[off + 1]]);
        let bar = u32::from_le_bytes([body[off + 4], body[off + 5], body[off + 6], body[off + 7]]);
        let eid = u32::from_le_bytes([body[off + 8], body[off + 9], body[off + 10], body[off + 11]]);
        out.push((dev, bar, eid));
    }
    Some(out)
}
```

- [ ] **Step 3b: Add the XI2 minor-61 arm** in `handle_xi2_request`'s `match minor` (before the `_ =>` catch-all at ~14132):

```rust
        61 => {
            if let Some(entries) = x11::parse_xi_barrier_release(body) {
                for (dev, bid, eid) in entries {
                    // Xorg ProcXIBarrierReleasePointer validates the device
                    // (dixLookupDevice) and that the barrier applies to it
                    // (GetBarrierDevice) → BadDevice (xibarriers.c:902,921,925).
                    // yserver's master pointer is device 2; the wildcards
                    // 0/1 also resolve to it. Anything else → BadDevice.
                    if !(dev == 0 || dev == 1 || dev == 2) {
                        return emit_x11_error_with_minor(state, client_id, sequence, XI1_ERROR_BAD_DEVICE, u32::from(dev), 61, XI2_MAJOR_OPCODE);
                    }
                    match state.pointer_barriers.get_mut(&bid) {
                        None => {
                            return emit_x11_error_with_minor(state, client_id, sequence, x11::error::BAD_VALUE, bid, 61, XI2_MAJOR_OPCODE);
                        }
                        Some(b) if b.owner != client_id => {
                            return emit_x11_error_with_minor(state, client_id, sequence, x11::error::BAD_ACCESS, bid, 61, XI2_MAJOR_OPCODE);
                        }
                        Some(b) => {
                            if b.event_id == eid {
                                b.release_event_id = eid;
                            }
                        }
                    }
                }
            }
        }
```

- [ ] **Step 3c: Add the release short-circuit** in the Task-7 clamp loop, at the marked spot right after `let key = keys[idx];` (Task 7 already snapshots `keys`): if `state.pointer_barriers.get(&key)` has `event_id == release_event_id`, `continue` (the `seen.push(idx)` above already advanced the loop) so this barrier is not clamped for the released crossing.

- [ ] **Step 4: Run, verify pass.** Run: `cargo test -p yserver-protocol parse_xi_barrier_release` and `cargo test -p yserver-core release_lets_pointer_cross`
Expected: PASS.

- [ ] **Step 5: fmt + clippy + commit.**

```bash
cargo +nightly fmt && cargo clippy -p yserver-core -p yserver-protocol
git add -A && git commit -m "feat(barriers): XIBarrierReleasePointer one-shot release"
```

### Task 11: BarrierLeave on delete-while-hit; window-destroy drop

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` (`DELETE_POINTER_BARRIER` arm — synthesize leave if hit)

- [ ] **Step 1: Write the failing test.** Delete a hit barrier; assert a `BarrierLeave` (evtype 26) with `flags = XIBarrierPointerReleased (1)`, `sourceid = 0`, zero `dx/dy` is delivered to a selecting client before removal.

```rust
    #[test]
    fn delete_while_hit_emits_released_leave() {
        // setup: barrier hit=true, client selected XI_BarrierLeave (bit 26).
        // delete by owner → expect a 68-byte GenericEvent evtype 26,
        // flags bit0 set, buf[40..42]==0 (sourceid), dx/dy zero.
    }
```

- [ ] **Step 2: Run, verify fail.** Run: `cargo test -p yserver-core delete_while_hit_emits_released_leave`
Expected: FAIL — no leave emitted.

- [ ] **Step 3: Implement.** Factor a helper `synthesize_released_leave(state, barrier_xid)` that, if the barrier is `hit`, builds a `BarrierLeave` via `write_xi_barrier_event` with `evtype = 26`, `flags = 1` (`XIBarrierPointerReleased`), `sourceid = 0`, `dx = dy = 0.0`, preserving `root`/`event_window`/`event_id`, and delivers it (same delivery helper as Task 9). Call it:
  - in the `DELETE_POINTER_BARRIER` `Some(_)` branch **before** `remove`;
  - in `process_disconnect` (`process_disconnect.rs`) for each `hit` barrier owned by the disconnecting client, **before** the `pointer_barriers.retain(...)` from Task 5 (Xorg `BarrierFreeBarrier` fires on the resource teardown that disconnect triggers).

> Window-destroy: no code change needed beyond what Task 9 already does — delivery looks up `barrier.window` and drops the event if absent. Add a regression test confirming a hit barrier whose window was destroyed still confines but emits no event (drive a motion after destroying the window; assert `pointer_root` clamps and `read_all_available` yields no GenericEvent).

- [ ] **Step 4: Run, verify pass.** Run: `cargo test -p yserver-core delete_while_hit window_destroy`
Expected: PASS.

- [ ] **Step 5: fmt + clippy + full test + commit.**

```bash
cargo +nightly fmt && cargo clippy -p yserver-core -p yserver-protocol
cargo test -p yserver-core -p yserver-protocol
git add -A && git commit -m "feat(barriers): released-leave on delete-while-hit; window-destroy drops events"
```

---

# Phase 4 — KMS input-thread resync (HW-gated)

> This phase only affects bare-metal KMS. Land it last and verify on hardware. It also fixes a latent drift in the existing confinement feature.

### Task 12: Input-thread SetPosition command

**Files:**
- Modify: `crates/yserver/src/input_thread.rs` (command enum + handling + `pending_motion` invalidation)
- Modify: `crates/yserver/src/kms/v2/backend.rs` (`warp_pointer_root` ~15987: send `SetPosition` to the input thread)

- [ ] **Step 1: Read the current control channel.** `grep -n "InputThreadCommand\|pending_motion\|cursor_x" crates/yserver/src/input_thread.rs`. Determine the current latch type (Pause/Resume). Document the chosen mechanism inline (see spec "cross-thread catch": an `AtomicU64` packing `x:i32|y:i32` + a dirty flag, or a small queue).

- [ ] **Step 2: Write a failing unit test** for the command plumbing — that publishing a `SetPosition` updates the thread state's `cursor_x/cursor_y` accumulator and clears `pending_motion` on the next iteration. (Test the `LibinputThreadState` step function directly if it's unit-testable; otherwise test the position-slot read/write in isolation.)

- [ ] **Step 3: Implement** the coordinate-carrying slot + `pending_motion` invalidation, and have `warp_pointer_root` publish the clamped position.

- [ ] **Step 4: Run, verify pass.** Run: `cargo test -p yserver input_thread`
Expected: PASS.

- [ ] **Step 5: fmt + clippy + commit.**

```bash
cargo +nightly fmt && cargo clippy -p yserver
git add -A && git commit -m "feat(barriers): KMS input-thread SetPosition resync (+ fixes confine drift)"
```

### Task 13: HW smoke (release gate)

> Not a code task — the verification gate. Per `feedback_vng_pass_not_hw_pass`, vng is not sufficient.

- [ ] **Step 1: Build + run on bee, dual-head, GNOME/Mutter** (per `feedback_hw_recipes_user_only` — coordinate; one agent per checkout; run via the proven tmux procedure).
- [ ] **Step 2: Verify** (a) pointer holds at a monitor seam, (b) a firm push crosses (release path fires), (c) no pointer trap, (d) hot-corner / edge-resistance behaves. Capture `RUST_LOG=info` and an `xtrace` of a real Xorg barrier session to diff wire/events (`feedback_xorg_is_the_de_facto_spec`).
- [ ] **Step 3: Record** the result in `docs/status.md` and tick the audit item in `docs/superpowers/findings/2026-06-17-tech-debt-stub-audit.md`.

---

## Self-review notes (for the executor)

- **Spec coverage:** resource model (T1), XID namespace (T2), wire parse (T3), create/delete + all 6 validation errors (T4), disconnect free (T5), clamp math (T6), motion hook + bypass (T7), event encode (T8), Hit/Leave + grab (T9), release one-shot (T10), delete-leave + window-destroy (T11), KMS resync (T12), HW gate (T13). The Xinerama-style multi-CRTC `window→screen` mapping is single-root-space per the spec non-goals.
- **Type consistency:** `PointerBarrier` field names are identical across T1/T2/T4/T5/T7/T9/T10/T11. `write_xi_barrier_event` signature defined in T8 is reused verbatim in T9/T11. `barriers::{BarrierGeom, direction_of, find_nearest, clamp_to_barrier, POSITIVE_X, NEGATIVE_X, ...}` defined in T6, used in T7/T10.
- **Known coarse steps:** T9 Step 3 is explicitly split into (a)(b)(c) sub-commits; T12 needs the executor to read the current input-thread channel first (Step 1) before the test, because the exact slot mechanism depends on what's there.
- **XID error = BadAlloc:** T4 uses `BAD_ALLOC` for an in-use/out-of-range barrier XID, matching Xorg `XICreatePointerBarrier`'s `AddResource` failure (xibarriers.c:827) and the spec's errors table. (This differs from the `CREATE_REGION` sibling, which uses `BadIdChoice` — barriers follow Xorg's barrier-specific behavior.)
```
