# XC-MISC extension — design

**Date:** 2026-06-12
**Status:** draft
**Branch:** `feat/xcmisc` (worktree `yserver-master`)

## Problem

yserver does not implement XC-MISC. When a long-lived client exhausts
its ~1M-XID range (20-bit per-client mask, `resource_id_mask =
0x000F_FFFF`; `server.rs` `PER_CLIENT_MASK`), libxcb's
`xcb_generate_id()` calls XC-MISC `GetXIDRange` to recycle freed IDs.
With the extension absent it returns the failure sentinel
`0xffffffff`; the client's next create request draws `BadIDChoice`,
and GDK-class clients treat X errors as fatal and exit cleanly.

Observed 2026-06-12 on bee (lightdm/Cinnamon): the WM died silently
at 1h43m, request serial 4.5M —
`client 34 CreatePixmap BadIDChoice pid=0xffffffff (out-of-range)` in
`target/x-0.log.old:30874`. Any compositing shell is a ~2h time bomb.
Also the prime suspect for the recurring "window vanishes from
yserver resources" bug (Firefox-heavy = high XID churn).

## Goal

Implement XC-MISC 1.1 (GetVersion, GetXIDRange, GetXIDList) matching
Xorg (`Xext/xcmisc.c`, `dix/resource.c:707-775`,
`/usr/include/X11/extensions/xcmiscproto.h`), so ID recycling works
for every client on every backend.

## Non-goals

- Migrating existing `xid_in_use()` callers (fresh-ID validation in
  CreateAnimCursor etc.) to the new comprehensive occupancy checker.
  That's a latent-bug fix of its own (a client could today create a
  pixmap over its own sync-counter ID) — follow-up, not this change.
- Per-request access control / XACE analogues.

## Protocol (from xcmiscproto.h — canonical)

Extension name: `"XC-MISC"`. No events, no errors. Version 1.1.

- **GetVersion (minor 0)**: req 8 bytes (`major(2), minor(2)` after
  the 4-byte header — values ignored, Xorg echoes its own); reply:
  32-byte fixed, `major(2)=1, minor(2)=1` at bytes 8..12, rest pad.
- **GetXIDRange (minor 1)**: req 4 bytes (header only); reply 32-byte
  fixed: `start_id(4)` at 8..12, `count(4)` at 12..16, rest pad.
  Semantics: a contiguous range of `count` XIDs starting at
  `start_id`, all inside the requesting client's `base..base|mask`
  space and none currently designating a live resource. Exhausted →
  wire shape `start_id = 0, count = 1` (see "Edge cases" — this is
  Xorg's exact encoding, not a typo).
- **GetXIDList (minor 2)**: req 8 bytes (`count(4)` after header);
  reply: variable — `length = count_found` (each XID is one 4-byte
  word), `count_found(4)` at bytes 8..12, pad to 32, then
  `count_found` XIDs.

Byte order: all fields through the existing `write_u16`/`write_u32`
helpers (`yserver-protocol/src/x11/wire.rs:32-51`) honoring the
client's `byte_order` — same as every other reply encoder.

### Edge cases (Xorg-faithful)

- Xorg's GetXIDRange returns `min=0, max=0` when the range is fully
  exhausted, which encodes on the wire as `start_id=0, count=1`
  (`max_id - min_id + 1`). XID 0 is never valid for creation, so
  clients treat it as exhaustion. We replicate the same wire shape
  (`start_id=0, count=1`) rather than inventing `count=0` —
  bug-compatibility beats prettiness (libxcb's exhaustion check is
  `start == 0`).
- GetXIDList with huge `count`: **explicit Xorg deviation.** Xorg
  pre-rejects `count > u32::MAX/4` with BadAlloc, then allocates
  `count × 4` bytes (multi-GB for hostile counts) and returns
  BadAlloc on allocation failure (`xcmisc.c:101-106`). We instead
  clamp the scan to the client's range size — the reply can never
  exceed `min(count, mask+1)` entries (≤ ~4 MiB of reply), so no
  unbounded allocation exists and no BadAlloc path is reachable by
  construction (Rust `Vec` growth of ≤4 MiB aborts only on true
  OOM, same as every other reply buffer in the server). Observable
  difference vs Xorg: a request for more IDs than the range holds
  gets `min(count, free-in-range)` instead of Xorg's identical-
  in-practice range-bounded answer, and absurd counts get a short
  list instead of BadAlloc. Rationale: never let a client make the
  server allocate gigabytes.
- Unknown minor opcode → **BadRequest** via
  `emit_x11_error_with_minor` (Xorg's dispatcher default,
  `xcmisc.c:129-141`) — NOT silently ignored.
- Length validation: the top-level exact-length table
  (`request_lengths`) is core-opcode-only, so XC-MISC validates
  inside `handle_xcmisc_request`: GetVersion req must be 2 units
  (8 bytes), GetXIDRange 1 unit (4 bytes), GetXIDList 2 units
  (8 bytes) — mismatch → BadLength via `emit_x11_error_with_minor`
  (match the RENDER arm pattern).

## Design

### Extension registration

- `nested.rs` `EXTENSIONS` table: add
  `("XC-MISC", major 152, first_event 0, event_count 0, first_error 0,
  ExtensionAvailability::Always)`. 152 is the next free major (128,
  130, 133-138, 140-151 taken; gaps 129/131/132/139 left alone to
  match the table's existing convention of not back-filling host-X11
  opcodes). QueryExtension and ListExtensions pick it up for free via
  `extension_query_reply` / `advertised_extension_names`.
- `process_request.rs` dispatch: `152 => handle_xcmisc_request(...)`.

### Comprehensive XID occupancy

New method on `ServerState` (it needs both the resource table and the
extension maps):

```rust
/// True iff `id` designates ANY live resource in ANY XID namespace.
/// XC-MISC must never report an occupied ID as free — the 8
/// ResourceTable maps (via `resources.xid_in_use`) plus the 9
/// extension maps that `xid_in_use` does NOT cover.
pub fn xid_occupied(&self, id: u32) -> bool {
    self.resources.xid_in_use(ResourceId(id))
        || self.xfixes_regions.contains_key(&id)
        || self.sync_counters.contains_key(&id)
        || self.sync_alarms.contains_key(&id)
        || self.sync_fences.contains_key(&id)
        || self.damage_objects.contains_key(&id)
        || self.mit_shm_segments.contains_key(&id)
        || self.glx_contexts.contains_key(&id)
        || self.glx_drawables.contains_key(&id)
        || self.present_event_selections.contains_key(&id)
}
```

Maintenance hazard: a future extension adding a 10th XID-keyed map
must extend this. Mitigation: doc comments on `xid_occupied` AND on
`xid_in_use` cross-referencing each other, plus a test that seeds one
resource of EVERY namespace and asserts occupancy (a new map without
coverage shows up in review against that test's pattern). When
auditing, note that not every `HashMap<u32, ..>` is an XID namespace:
maps keyed by client ids, host xids, or internal handles
(`zombie_clients`, `host_glyphset_refcounts`, host-side caches) are
NOT client-XID-keyed and don't belong in `xid_occupied` — the test
seeds only maps whose keys arrive from client `CARD32` resource-id
fields on the wire.

Zombie clients (RetainPermanent/Temporary) need no special handling:
their bases are never released by `IdAllocator`, so a live client's
range cannot contain another owner's resources. GetXIDRange/List
operate purely within the requester's own `base..base|mask` window —
ownership filtering is unnecessary; occupancy is sufficient.

### GetXIDRange algorithm

Xorg walks the client's own resource buckets and shrinks a
`[id..maxid]` window via `AvailableID` probes
(`dix/resource.c:707-737`). We deliberately use a DIFFERENT,
protocol-conformant implementation (the spec promises only "a
contiguous range of unused IDs", not a particular one): collect the
client's used IDs once, sort, and scan gaps. Not the same algorithm
or cost profile as Xorg — an implementation choice, traded for
simplicity against our map-based table:

```rust
fn free_xid_range(state: &ServerState, base: u32, mask: u32) -> (u32, u32) {
    let lo = base.max(1); // XID 0 is never allocatable
    let hi = base | mask;
    let mut used: Vec<u32> = state.used_xids_in(base, mask); // sorted
    // scan [lo..hi] gaps between used ids; return the LARGEST gap
    // as (start, count); none free → (0, 1) per the edge-case note.
}
```

`used_xids_in(base, mask)` iterates the 17 maps' keys filtering by
`(key & !mask) == base` — O(total resources), matching Xorg's cost.
Returning the largest gap (vs Xorg's "whatever the shrink loop
leaves") is strictly better for the client and protocol-conformant:
the spec promises only "a range of unused IDs".

### GetXIDList algorithm

Xorg's brute scan, bounded: walk `id` from `max(base,1)` to
`base|mask`, collect ids where `!xid_occupied(id)`, stop at
`min(count, mask+1)` found. Worst case 2M `xid_occupied` probes
(17 HashMap lookups each) on a degenerate request — acceptable for a
"very rare" path (Xorg's own comment), and the loop short-circuits
once `count` is found, which for libxcb is always small.

### Handler shape

`handle_xcmisc_request` in `process_request.rs`, modeled on
`handle_render_request`: `match header.data { 0 => .., 1 => .., 2 => ..,
other => BadRequest via emit_x11_error_with_minor }` (Xorg dispatcher
default — see Edge cases). Reply encoders
`write_xcmisc_get_version_reply` / `..get_xid_range_reply` /
`..get_xid_list_reply` in `yserver-protocol/src/x11/mod.rs` next to
the other fixed-size encoders, using `write_u16`/`write_u32`.

### Backend interaction

None. Pure core. ynest and KMS get it identically — the resource
table is the single source of truth on both.

## Testing

1. **Unit (process_request tests mod, existing harness):**
   - GetVersion: reply bytes = 1.1, correct sequence/major opcode.
   - GetXIDRange on a fresh client: returns `(base.max(1),
     full-range count)`.
   - GetXIDRange with seeded occupancy: seed resources splitting the
     range, assert returned range is the largest gap and contains no
     occupied id.
   - **Cross-namespace occupancy:** seed ONE resource from EACH of
     the 17 namespaces at known ids; assert `xid_occupied` true for
     every one of them and GetXIDList skips exactly those ids. (The
     guard test for the maintenance hazard.)
   - GetXIDList: exact count returned, ids ascending, none occupied,
     all in-range; huge-count clamp; exhausted range → empty list.
   - GetXIDRange fully-exhausted edge → wire shape `start_id=0,
     count=1`.
   - BadLength for malformed request sizes (all three minors).
   - Unknown minor (e.g. 3) → BadRequest with minor echoed.
   - QueryExtension "XC-MISC" → present, opcode 152; ListExtensions
     contains it.
2. **Exhaustion smoke (`tools/xid-exhaust-probe.c`):** xcb client
   that loops create/free pixmap >2^21 times (burning the ID space),
   then asserts `xcb_generate_id()` still returns a valid id and a
   final CreatePixmap+GetGeometry round-trip succeeds. Run against
   ynest (backend-independent bug; fastest loop). Pre-fix: probe
   FAILS with generate_id()==0xffffffff (reproduces bee incident);
   post-fix: PASSES. Optional control run against Xephyr/Xorg.
3. **HW/dogfood:** none required beyond normal soak — the bee
   evidence run (a >2h Cinnamon session) is the real-world validation
   and will happen organically.

## Risks

- Missing a current XID namespace in `xid_occupied` → hands out
  colliding ids (the original bug, reintroduced subtly). Mitigated by
  the cross-namespace seeded test; reviewers should independently
  grep `HashMap<u32,` in server.rs/resources.rs against the list.
- Future XID namespaces silently uncovered (see maintenance hazard).
- `used_xids_in` collection cost per GetXIDRange call — O(total live
  resources); calls are rare (only at exhaustion), no caching needed.
