# Server reset — implementation plan

Implements `../specs/2026-09-09-server-reset-design.md`. Read that first; this
plan does not restate its reasoning, only what to build, in what order, and
what proves each part.

**Branch:** `feat/121-server-reset`, off `feat/121-tcp-transport` (stage 1 is
unmerged and this builds on its `Transport`/generation-aware client handling).

**Deliverable:** a server started with `-reset` runs two consecutive `xterm`
sessions. The second sees none of the first's atoms, properties, selections or
pixels, input still works in it, and the display never blinks between them.
With `-noreset` — the default — behaviour is byte-identical to today.

## Ordering principle

**Nothing can fire until everything is built.** The trigger lands *last*
(step 5), so steps 1-4 are inert additions that cannot reset anything, and
each is provable on its own. That inverts the tempting order — wiring the
trigger early to "see it work" would mean debugging a half-built quarantine
against live sessions.

Within that: inventory before state (step 1 supplies what step 4 seeds from),
tagging before cleanup (step 2 is what makes step 3's destruction safe against
in-flight traffic), and cleanup before the boundary that calls it.

## Prerequisites

- `cargo +nightly fmt`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` clean before each commit.
- **No xts A/B.** Reset *does* draw — it deliberately clears the scanout — but
  the xts gate exists for changes to rendering behaviour, and nothing here
  alters how anything is drawn. The scanout claim is checked by direct
  assertion instead, split across two steps (below).
- **Hardware only for step 6.** Steps 1-5 are loop and state work, testable in
  the sandbox — including step 4's scanout obligation, discharged there
  against the **recording backend** (assert the clear/dirty path is invoked)
  and only becoming a real framebuffer check in step 6.

---

## Step 1 — `InputInventory`, process-lifetime

The seed source. Purely additive; nothing consumes it yet.

- `InputInventory`: `HashMap<DeviceNode, DeviceInfo>`, owned beside the core
  loop, **not** in `ServerState`.
- Populated from `Message::HostInput`'s `DeviceAdded(info)` and cleared on
  `DeviceRemoved { device_node }` (`input/context.rs:314,331`).
- Atom-free by construction — device facts only, no interned property atoms,
  no XI ids. Assert that in review: the moment an atom lands in here, the
  cross-generation guarantee is gone.

**Proof.** Unit: add/remove sequences leave the expected set; a duplicate
`DeviceAdded` for one node replaces rather than duplicates. No behaviour
change anywhere else — the existing suite must pass untouched.

## Step 2 — generation counter and message tagging

Inert while the generation never increments.

- A generation counter beside the loop; every `Message` carries the generation
  it was produced in.
- Discard on mismatch at the top of dispatch, **session-scoped only**:

  | Scope | Variants |
  |---|---|
  | Session — discard on mismatch | `SetupAllocate`, `ClientSetupComplete`, `Request`, `ClientDisconnected` |
  | Process lifetime — always process | `HostInput`, `CrtcConfigReady`, `Shutdown`, `VtRelease`, `VtAcquire`, `SwitchVt`, `DumpScanout`, `DumpDrawables` |

  That is the complete variant list (`core_loop/message.rs:124`); if a variant
  is added later it must be classified explicitly, so make the match
  exhaustive rather than defaulting.

  **`CrtcConfigReady` is process-lifetime for a specific reason**, not by
  omission: it is only a wake. `drain_ready_crtc_configs` (`run.rs:942`) looks
  the token up in the pending map and, finding nothing — or finding the
  originating client gone — calls `backend.cancel_crtc_config(token)` and moves
  on. A completion arriving after a reset is therefore already discarded by
  existing code, and tagging it would only risk dropping a *live* wake.

**Proof.** Unit: a session-scoped message tagged with an old generation is
dropped; a process-lifetime one is processed. **The test that matters**: a
`DeviceRemoved` tagged old is still applied to the inventory — discarding it
would corrupt the inventory permanently, since the device is gone and no
second notification is coming. Also: a `CrtcConfigReady` bearing a token parked
by the *previous* generation must be cancelled, not mistaken for a newly
pending operation — the harmless already-cancelled case, since step 4 cancelled
it at the boundary. With the counter pinned at 0 the whole existing suite must
pass unchanged.

## Step 3 — the forced cleanup path

Destroys a session. Not yet called by any reset.

- `force_destroy_all_clients(state, backend)`: closes every live client
  **ignoring close-down mode**, unlike `process_disconnect`
  (`process_disconnect.rs:89`, `let retain = close_mode == 1 || close_mode == 2`).
- Explicitly destroys existing zombie/retained clients' resources.
- Releases **backend-side** objects — host pixmaps, GLX contexts and
  drawables, DRI3 syncobjs, host-window registrations (`HostXidMap`). This is
  the #133 `host_xid_still_referenced` lifetime class; core metadata
  disappearing is not the same as the backend freeing anything.

  Two corrections to an earlier draft of this list, found in implementation:
  **"registered writers" named nothing** — there is no writer registry on the
  `Backend` trait; client writers are core-side in `ServerState.clients`. And
  **`GlxContext` is core-only** — the sole backend-side GLX lifetime is the
  pixmap-export refcount taken at `glXCreatePixmap`, so a context has nothing
  to release.

  **The per-client orphan gate is not sufficient on its own.** It is
  per-*reference*: client A's tile stays allocated while client B's GC names it
  as tile/stipple, which is the right answer in a normal disconnect because B
  is still running. In a reset B dies too and nothing reports that tile a
  second time — `remove_non_window_resources_owned_by` yields only pixmap
  resources, and `collect_attribute_pixmap_host_xids` covers window background
  and border only. So the teardown must collect deferrals, subtract anything
  freed later, and re-check each survivor against `host_xid_still_referenced`
  once the whole session is gone.

**Proof.** Unit, called directly: a session that allocated host pixmaps, GLX
objects and DRI3 syncobjs leaves the **backend's own accounting empty** — not
merely `ServerState`. A client with `RetainPermanent` is destroyed like any
other. Verify red by asserting against backend accounting before writing the
release calls; a `ServerState`-only assertion passes without them and would
wire in the leak.

## Step 4 — `reset_generation`, the boundary

Assembles steps 1-3. Still unreachable — nothing calls it outside tests.

Order, per the spec:

1. Bump the generation.
2. Cancel pending setup handshakes — `shutdown(Both)` every `SetupRegistry`
   entry, reusing `setup_thread.rs:102` `shutdown_all`.
3. `force_destroy_all_clients`, deregistering each from the poller.
4. **Clear the per-client state that lives outside `ServerState`.** The spec
   says "queued requests"; concretely:
   - the parked-CRTC maps (`crtc_by_client`/`crtc_by_token`, `run.rs:554`) —
     but **cancel before clearing**: take every parked token, call
     `backend.cancel_crtc_config(token)` on each, *then* empty the maps. The
     token is the only handle to the backend operation, so clearing first
     leaks any parked config permanently. `drain_ready_crtc_configs` cannot
     recover it either: that path runs only when a completion arrives, and a
     parked op may never deliver one.
   - the fair deferred-request queue (`by_client`/`ready`, `run.rs:625`);
   - **`server_grab_waiters`** (`run.rs:690,838`) — a *separate*
     `VecDeque<DeferredRequest>` alongside the fair queue, and the one with
     teeth: `release_server_grab_waiters` (`run.rs:688`) pushes its contents
     back into `deferred_requests` when the server grab releases. Clearing only
     the fair queue leaves a destroyed client's requests eligible to be
     *restored into the fresh generation*.

   Not `channel_requests_by_client`: it is a stack-local telemetry map created
   per `NOTIFY_TOKEN` drain (`run.rs:1272`) and gone immediately after, so
   there is nothing to clear, and listing it would send an implementer hunting
   for inaccessible state.

   **Audit for others before implementing** — this list came from grepping for
   per-client collections and is not guaranteed complete; anything missed is a
   cross-session leak.
5. Cancel any remaining in-flight backend operations for destroyed clients.
   (The parked CRTC configs are already handled in 4 — they have to be, since
   their tokens live in the maps that step clears.)
6. Replace `*state` with a fresh `ServerState`, seeded from live backend
   topology (outputs, modes, providers — cf. `lib.rs:392,395`) and from the
   `InputInventory`, interning device-property atoms anew.
7. `install_backend_root_bindings` (`lib.rs:29`).
8. Rebind the root window, mark the backend dirty, **clear the scanout**.
   The leaked COW claim is **not** handled here. Decided 2026-09-09: it needs
   the structural per-client-resource fix in the spec's "Adjacent gaps", which
   is a prerequisite for shipping reset rather than a part of it. A bounded
   decrement loop was written here and rejected — arbitrary cap, and on a
   teardown failure it carried the old compositor's claim into the next
   session.
9. Listeners untouched.

**Proof.** Unit: after a direct call, the new state has empty
resources/atoms/selections/grabs; `start_instant` is unchanged; the device set
is present again with property atoms interned in the *new* table. Quarantine:
begin a setup handshake, reset before it completes, assert no client appears
and the handshake socket is closed; and a request left in `server_grab_waiters`
by a destroyed client is not restored when the new generation's server grab
releases. Scanout, in the sandbox: assert reset invokes the recording backend's
clear/dirty path — "the old pixels are actually gone" is step 6, on hardware.

## Step 5 — flags, signal and the armed trigger

The first step in which a reset can happen.

- Parse `-noreset` (default), `-reset`, `-terminate`; `SIGHUP` forces one.
- `reset_armed`, per generation, initially **false**, set when a client is
  registered at `run.rs:2913` — the single production site where a client
  becomes established.
- Fire only when a client disconnects **and** `reset_armed` **and** no clients
  remain. Never from a state check: an idle client set is indistinguishable
  from a drained one, and a `-reset` server would otherwise reset itself
  repeatedly at startup.

**Proof.** The trigger table: last client leaves × each flag, SIGHUP in each
mode, and `RetainPermanent` (must not inhibit). **Arming cases, which a
happy-path suite misses:** an idle `-reset` server never resets; a client that
drops before completing setup arms nothing; a client refused for a bad cookie
arms nothing. That last one is reachable by strangers now that stage 1 binds
`0.0.0.0`.

## Step 6 — end to end

- Two consecutive `xterm` sessions on one server with `-reset`: the second
  starts clean, input works, the screen does not retain the first's contents,
  and the display does not blink at the boundary.
- Default (`-noreset`) unchanged: run a normal `just startx` session and
  confirm nothing resets when a client exits.
- `docs/man/yserver.1.scd` for the three flags and SIGHUP; `docs/status.md`.

## Hazards

- **The quarantine has no happy-path symptom.** A reset with no concurrent
  connection attempt looks identical whether or not stale traffic is
  discarded. Step 4's failing case must be constructed deliberately.
- **Backend leaks are invisible to `ServerState` assertions.** Step 3's proof
  must assert against backend accounting or it proves nothing.
- **The out-of-`ServerState` per-client maps are the likeliest miss.** They are
  scattered across `run.rs` rather than collected, so the audit in step 4.4 is
  the real work of that step, not the clearing.
- **Seeding order.** The new state must intern device-property atoms *after*
  its atom table exists; reseeding input before the state is built, or reusing
  the old atom ids, reintroduces exactly the dangling-atom bug the spec's
  survive list was corrected for.
- **`-noreset` must stay the default.** Inheriting Xorg's reset-on-last-client
  would turn a momentarily empty client set into what looks like a crash for
  every `starty` and `just *-hw` user.
- Test fixtures construct `ServerState` at ~46 sites; step 4 changes how it is
  built in production but must not quietly change what the fixtures produce.
