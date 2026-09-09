# Server reset — design

## Status

Draft, unimplemented. Written 2026-09-09 against master `64d4b6e4`.

**Revised after codex review (2026-09-09), which withheld approval on four
blocking gaps — all verified against the source and all accepted:**

1. The boundary must be a **generation quarantine**, not a `ServerState` swap:
   setup threads, reader threads, queued messages, poll registrations and
   in-flight backend work all live *outside* `ServerState`.
2. The survive list was **self-contradictory**. Carrying `xi_devices` while
   destroying `atoms` leaves stale atom IDs, against this spec's own atom
   invariant. Only `start_instant` survives literally.
3. The trigger must remember that the generation **had** a running client.
4. Reset needs a **forced** cleanup path; the normal disconnect honours
   `RetainPermanent`, and a state swap frees no backend-side objects.

**Second review round (2026-09-09), one further blocker, accepted:** "seed from
the live input layer" had no owner. `probe_input_devices` is a **no-op in
Direct mode** (`run.rs:1075`) — libinput lives on the input thread and its
`DeviceAdded` burst is one-shot at process start — so a generation that
discards `xi_devices` and re-probes gets **no input devices at all**. Resolved
with a process-lifetime `InputInventory`; see its section. Message tagging was
also tightened from "every message" to session-scoped only, since discarding a
`DeviceRemoved` mid-reset would corrupt that inventory permanently.

Stage 3 of four for [#121](https://github.com/joske/yserver/issues/121), taken
**before** stage 2 (`docs/…/2026-09-09-host-access-control-design.md`) because
stage 2 unblocks nothing — XDMCP authorizes by cookie — while this stage is a
hard requirement: LightDM resets the X server after logout, and XDMCP's own
session loop is built on it (`os/xdmcp.c:651,808` raise `DE_RESET`).

1. ~~TCP transport~~ — done, `feat/121-tcp-transport`
2. ACL — specified, deferred
3. **Server reset** ← this spec
4. XDMCP itself

## Goal

When a session ends — the last client that reached Running disconnects, or
SIGHUP arrives — destroy every trace of it and return the server to a
freshly-started state, without exiting the process and without disturbing the
display, input devices or the listening sockets. Then accept the next session.

"Every trace" includes state outside `ServerState`: in-flight setup
handshakes, reader threads, queued messages, and backend-side objects. See
"The generation boundary".

**Non-goals:**

- Re-probing KMS or re-initialising Vulkan. Xorg re-runs `InitOutput` on every
  generation; we deliberately will not. The display must not blink between
  sessions, and our backend init is expensive. Outputs and modes are re-derived
  from the live backend, not re-discovered.
- Session *persistence* of any kind. A reset is a full erasure; nothing
  client-created survives by design.
- The XDMCP re-query loop (stage 4). This stage provides the hook it will use.

## Current behaviour on master

There is **no reset**. Nothing observes the last client leaving — no
`clients.is_empty()` check exists anywhere in the core loop — and
`-noreset`/`-reset`/`-terminate` are not parsed. The server runs until killed,
so a second session on one server is impossible today.

Two structural facts make this tractable:

- **`ServerState` is not owned by the core loop.** It is built at
  `crates/yserver/src/lib.rs:395` via
  `ServerState::with_randr_outputs_and_modes(...)` and threaded through as
  `state: &mut ServerState`, so it can be replaced wholesale rather than
  cleared field by field.
- **The backend is separate** and already re-binds through
  `install_backend_root_bindings(state, backend)` (`lib.rs:29`), which is
  exactly the re-attachment a new generation needs.

That makes replacement the right *core* of the boundary — with 89 public
fields, a fresh construction resets all of them by default, including ones
added years from now that a hand-written `clear()` would silently retain. But
replacement is only the core: the generation also owns setup threads, reader
threads, queued messages, poll registrations and backend-side objects that no
`ServerState` swap touches. Treating the swap *as* the boundary is the first
draft's central error.

## Reference: how Xorg does it

`dix/main.c`'s `main` is a `while (1)` loop; one iteration is one *generation*:

```
142  while (1) {
162      ResetWellKnownSockets();
184      InitAtoms();                 /* calls FreeAllAtoms() first */
190      InitOutput(&screenInfo, argc, argv);
217      CreateRootWindow(pScreen);
248      InitInput(argc, argv);
274      Dispatch();                  /* the entire session */
285      CloseDownExtensions();
292      FreeAllResources();
306      CloseDownDevices();
337      FreeAllAtoms();
345      if (dispatchException & DE_TERMINATE) break;
349      OsCleanup(...);
     }
```

**The trigger** — `CloseDownClient` (`dix/dispatch.c:3537`):

```c
if (client->clientState == ClientStateRunning && nClients == 0)
    SetDispatchExceptionTimer();
```

i.e. the last client that reached *Running* disconnecting. `really_close_down`
gates it, so a client that set `RetainPermanent` via `SetCloseDownMode` does
not count as gone.

**The policy** — `dispatchExceptionAtReset` (`dix/dispatch.c:3480`), default
`DE_RESET`; `-noreset` sets it to `0` (never reset), `-reset` back to
`DE_RESET`, and `-terminate` to `DE_TERMINATE` (exit instead), with an optional
delay (`SetDispatchExceptionTimer`, `:422`). **SIGHUP** also forces a reset via
`AutoResetServer` (`os/utils.c:407`).

**Atoms are destroyed.** `InitAtoms` at `:184` and `FreeAllAtoms` at `:337`, so
each generation starts with only the predefined atoms. Worth stating because
it is the field most likely to be assumed persistent.

## Design

### The generation boundary

A `ServerState` swap is **not** the boundary. Substantial generation-owned
state lives outside it: setup threads and the `SetupRegistry`, per-client
reader threads, messages already queued on the core channel, mio poll
registrations, and in-flight backend work. The concrete leak: a setup
handshake that began before the reset still sends `Message::ClientSetupComplete`
(`run.rs:1318`), and `handle_client_setup_complete` (`run.rs:2893`) inserts the
client into whatever generation is current and spawns its reader — so a client
authorized against the *previous* session appears in the next one.

So the server carries a **generation counter**, and `reset_generation(state,
backend, …)` performs, in order:

1. **Bump the generation.** Everything below is defined relative to it.
2. **Cancel pending setup handshakes** — walk the `SetupRegistry` and
   `shutdown(Both)` each entry (the mechanism `setup_thread.rs:102`
   `shutdown_all` already implements), so no new client can complete into the
   new generation.
3. **Force-close every established client** via the reset-specific path below,
   deregistering each from the poller and joining or detaching its reader.
4. **Discard stale *session-scoped* channel traffic.** Tag messages with the
   generation that produced them and ignore mismatches at the top of the
   dispatch loop — safer than draining, which races with producers still
   running. Tagging must be **exact, not blanket**:

   | Scope | Messages | On mismatch |
   |---|---|---|
   | Session | setup allocate/complete, client requests, disconnects, async backend completions | discard |
   | Process lifetime | `DeviceAdded`/`DeviceRemoved`, signals, backend hotplug | **always process** |

   Discarding a `DeviceRemoved` because a reset happened to be in flight would
   corrupt the `InputInventory` permanently — the device is gone and no second
   notification is coming.
5. **Cancel in-flight backend operations** for the destroyed clients, so no
   completion lands against a resource id that now means something else.
6. **Replace `*state`** with a freshly constructed `ServerState`, seeded per
   "Seeding the new generation" below.
7. **Re-run `install_backend_root_bindings`.**
8. **Rebind the root window and explicitly clear it.** The fresh constructor
   creates a root window, but that is not sufficient: the reset must rebind it
   through `install_backend_root_bindings` *and* mark the backend dirty and
   clear the scanout. Building a correct new state while leaving the previous
   session's pixels on screen is easy, and under XDMCP those pixels belong to
   a different user.
9. **Leave the listeners bound and untouched.**

Steps 2-5 are the quarantine, and they are the part a naive implementation
skips. The listeners staying up (9) means new connections can arrive *during*
the reset; they must be accepted into the new generation or not at all, never
into a half-built one.

Constructing fresh rather than clearing in place is the whole point: a new
field added to `ServerState` next year is then reset correctly by default,
where a hand-written `clear()` would silently retain it.

### Seeding the new generation — what survives, and what is re-derived

The first draft listed `randr`, `xi_devices` and `glx_tfp_supported` as fields
to carry across. That was wrong, and in one case self-contradictory:

- **`xi_devices` cannot be carried.** Its own documentation
  (`server.rs:1251`) says property-name atoms "are interned via `self.atoms`
  so they share the same atom namespace as all other server atoms". Carrying
  the device registry while destroying the atom table leaves every device
  property pointing at an atom id that no longer exists — a direct violation of
  the atom invariant three paragraphs above it.
- **`randr` cannot be carried.** `RandrState` is entangled with client state:
  `randr_primary_output_explicit`'s own doc says "Rebuilds preserve only
  explicit choices", and startup applies provider topology separately
  (`lib.rs:392`). A generation must *reconstruct* it from backend topology, not
  inherit the previous session's view of it.
- **`glx_tfp_supported` is a constructor input**, not a survivor — it belongs
  in the backend-capability arguments the fresh state is built from.

So the rule is: **exactly one field survives literally.**

| Field | Why |
|---|---|
| `start_instant` | The server timestamp epoch. X11 timestamps must not go backwards; a client reconnecting a millisecond after a reset would otherwise see the clock jump. Nothing else about it is session state. |

Everything else that must be *true* of the new generation is **seeded**, from
an explicit snapshot taken from the live backend and input layer at reset time:
output/mode/provider topology, the physical device inventory, and backend
capabilities. Seeding differs from carrying in the way that matters here: the
new state interns its own atoms, allocates its own ids, and builds its own
`RandrState`, so no identifier crosses the boundary.

Notable members of the destroyed set, called out because a reader might expect
them to persist:

- **`atoms`** — per Xorg (`main.c:184,337`). Back to predefined only.
- **`dpms`, `screensaver`, `keyboard_control`, `pointer_control`,
  `repeat_state`** — client-settable server preferences. Xorg's `InitInput`
  restores defaults each generation; so do we. A session that set a 10-minute
  blank must not leak into the next user's session.
- **`keys_down`, `buttons_down`, `last_xkb_group`, `last_xkb_mods`** — logical
  input state. Reset to empty; the physical device is untouched, but a stale
  held-key bitmap is exactly the saturation failure already known from the XI
  grab work.
- **`id_allocator`, `close_down_modes`, `zombie_clients`** — a retained
  resource does not survive the reset that erases the session it was retained
  in.

### `InputInventory` — the seed source for input, and who owns it

"Seed from the live input layer" was an interface-shaped hole: **there is no
re-probe available at reset time.** `backend.probe_input_devices` is a no-op
for Direct mode and for host-X11/nested (`run.rs:1075` and the comment above
it), because in Direct mode the libinput `Context` lives on the dedicated
input thread, which dispatches the initial enumeration and emits its
`DeviceAdded` burst as its very first action before entering its epoll loop.
That burst is **one-shot, at process start**. A generation that discards
`xi_devices` and then tries to re-probe comes back with no input devices at
all.

So the inventory must be **process-lifetime**, not per-generation:

- `InputInventory`: `DeviceInfo` keyed by device node (`/dev/input/eventN`),
  living beside the loop rather than in `ServerState`.
- Maintained by the existing messages — inserted on `DeviceAdded(info)`,
  removed on `DeviceRemoved { device_node }` (`input/context.rs:314,331`).
- **Atom-free by construction.** It holds device facts only; no interned
  property atoms, no XI ids. That is what lets it cross a generation boundary
  when `xi_devices` cannot.
- A reset builds fresh XI state in the new `ServerState` and **reseeds it from
  the inventory**, interning property atoms anew in the new atom table.

This gives the snapshot an owner, an update path and a synchronisation model,
and it closes the standing `TODO(direct-mode startup probe)` as a side effect:
the same inventory records the burst whether or not anyone was ready for it.

### Forced cleanup, not the normal disconnect path

`process_disconnect` deliberately honours close-down mode
(`process_disconnect.rs:89`: `let retain = close_mode == 1 || close_mode == 2;`),
keeping zombie resources alive on purpose. Reusing it for a reset would be
wrong twice over: retained resources would survive the erasure, and replacing
`ServerState` afterwards would drop the *core* metadata while leaving the
**backend-side** objects — host pixmaps, GLX contexts and drawables, DRI3
syncobjs, host-window registrations — allocated with nothing left to
reference them.
That is precisely the host-pixmap lifetime class fixed under #133 behind
`host_xid_still_referenced`.

So reset requires its own path:

1. Force-close every live client **ignoring close-down mode**, releasing all
   backend-side resources.
2. Explicitly destroy every existing zombie/retained client's resources.
3. Only then replace the state.

"A mass disconnect is the same path at once" — as the first draft put it — is
not sufficient and is the sentence that hid this.

### The trigger must be armed, not inferred

"When the last client disconnects" is the wrong formulation: at startup the
client set is *also* empty, so a state-check implementation on a `-reset`
server would reset itself repeatedly while idle, before anyone ever connects.
Xorg avoids this by making the trigger an **event** inside `CloseDownClient`
with two conditions (`dix/dispatch.c:3537`): the departing client reached
`ClientStateRunning`, and the count is now zero.

Mirror that, and make it explicit rather than emergent:

- Each generation carries a `reset_armed` bit, initially **false**.
- It is set to true when a client completes setup and reaches Running — not at
  accept, and not at connect.
- The reset fires only when a client disconnects **and** `reset_armed` is true
  **and** no clients remain.

So an idle server never resets, and a client that connects and drops before
completing setup — a failed cookie, a port scan on the TCP listener — arms
nothing. The TCP listener makes that second case reachable by strangers, which
is why it is a requirement and not a nicety.

### Flags and signals

- `-noreset` — never reset; the current behaviour, and the right default for a
  single-session desktop launched by `starty`.
- `-reset` — reset on last client (Xorg's default).
- `-terminate` — exit instead of resetting.
- `SIGHUP` — an **explicit forced reset that overrides `-reset` and
  `-terminate`**, matching Xorg: `AutoResetServer` (`os/utils.c:407`) raises
  `DE_RESET` unconditionally, while `dispatchExceptionAtReset`
  (`dix/dispatch.c:3480`) is consumed only by the last-client path through
  `SetDispatchExceptionTimer` (`:422`). So Xorg resets on HUP even under
  `-terminate`, and we keep that: `-terminate` means "terminate when the last
  running client leaves", not "make every reset a terminate".

  **Our HUP behaviour is therefore deliberately non-uniform**, and that must be
  documented in the man page rather than discovered: it **shuts down** under
  `-noreset` and **resets** under `-reset` and `-terminate`. The `-noreset`
  case is a compatibility exception — SIGHUP shuts the server down today, and
  adopting Xorg's meaning there would turn a signal that currently stops a
  default server into one that destroys its session.

**Our default is `-noreset`** — reset only when `-reset` is passed, or implied
by the XDMCP options in stage 4. Recommended here and concurred by codex.

This inverts Xorg, which defaults to resetting on last client. The reason:
`starty` and every `just *-hw` recipe launch the server expecting it to
outlive its clients, so an inherited Xorg default would turn a momentarily
empty client set into what looks exactly like a crash. The divergence is in the
safe direction — the failure mode of wrongly not resetting is a stale session,
the failure mode of wrongly resetting is destroying a live one — and it matches
how the server is actually started today.

## Invariants

1. A reset never exits the process and never touches KMS, Vulkan or the
   listening sockets.
2. After a reset the server is indistinguishable, to a new client, from a
   freshly started one — except that timestamps continue to increase.
3. No client-created resource, atom, selection, grab, property, colormap or
   preference survives.
4. With `-noreset` — the default — behaviour is byte-identical to today: the
   last client leaving changes nothing.
5. A client holding `RetainPermanent` delays nothing, and its resources —
   core **and** backend-side — are destroyed by the forced path.
6. **No old-generation object, mapping or queued operation is ever
   interpreted as belonging to a newly reused numeric id.** Numeric ids *are*
   reused, deliberately and unavoidably — the root XID is fixed, a fresh
   `IdAllocator` reissues the same resource bases, and a fresh atom table
   assigns the same numbers. The guarantee is therefore semantic, not
   numeric, and it is what the quarantine and generation tagging exist to
   provide. Nothing here requires ids to be unique across generations.
   Atoms, resource ids and device-property atoms are all freshly interned or
   allocated, and the new state is seeded from hardware rather than from the
   previous state's fields.
7. A setup handshake in flight across a reset never produces a client in the
   new generation.
8. An idle server never resets: `reset_armed` is false until some client
   reaches Running.

## Risks

- **The quarantine is the part that will be under-implemented.** Steps 2-5 have
  no visible symptom in a happy-path test: a reset with no concurrent
  connection attempt looks identical whether or not stale traffic is discarded.
  The failing case needs deliberate construction — begin a setup, reset before
  it completes, assert no client appears.
- **Backend-side leaks are invisible to `ServerState` assertions.** A test that
  checks the new state is empty passes while host pixmaps, GLX objects and
  DRI3 syncobjs from the old session remain allocated. Assert against the
  backend's own accounting, the way #133's `host_xid_still_referenced` work had
  to.
- **The 89-field survive/destroy decision is silent when wrong.** A field that
  should survive but is reset shows up as a subtly broken second session; one
  that should reset but survives is a cross-session leak between *different
  users* under XDMCP, which is the serious direction. The fresh-construct
  design makes destroy the default, which is the safe default, but the
  single-entry survive list (`start_instant`) and every seeding input must be
  justified and reviewed as a unit.
- **Backend re-binding.** `install_backend_root_bindings` re-attaches the
  backend to the new state; anything else the backend caches by reference into
  the old state is a dangling assumption. Audit the backend for retained
  state-derived handles before implementing.
- **Root window recreation** must repaint. A reset that leaves the previous
  session's pixels on screen is both a visual bug and an information leak to
  the next user — the exact scenario XDMCP creates.
- **Reset during in-flight GPU work.** Present/DRI3 completions may be pending
  for clients being destroyed. The existing teardown paths handle per-client
  disconnect; a mass disconnect is the same path at once, but it has never been
  exercised.
- **Timestamps.** `start_instant` surviving is load-bearing; if a fresh
  `ServerState` reset it, every reconnecting client would see the server clock
  jump backwards.

## Verification

- Unit: a reset with N clients connected disconnects all of them; a fresh
  state has empty resources/atoms/selections/grabs; the survive-list fields are
  preserved; `start_instant` is unchanged.
- The trigger table: last client leaves × `-noreset`/`-reset`/`-terminate`, plus
  SIGHUP in each mode, plus a client with `RetainPermanent` (must not inhibit).
- **Arming cases, which a happy-path suite misses:** an idle `-reset` server
  never resets; a client that connects and drops *before* completing setup
  arms nothing and triggers nothing; a client refused for a bad cookie likewise.
- **Quarantine:** begin a setup handshake, reset before it completes, assert no
  client exists in the new generation and the handshake's socket is closed.
  Same for a `Message::Request` queued from a reader whose client is destroyed
  mid-reset.
- **Backend accounting:** after a reset with a session that allocated host
  pixmaps, GLX objects and DRI3 syncobjs, assert the backend holds none — not
  merely that `ServerState` is empty.
- **Input survives the reset.** The case that fails hardest if the inventory is
  missed: reset with devices present, then assert the new generation exposes
  the same device set via `XIQueryDevice`, with property atoms interned in the
  *new* atom table. Plus a `DeviceRemoved` delivered during a reset, asserting
  the inventory drops it rather than discarding the message.
- **Scanout is cleared**, not merely re-bound: assert the framebuffer does not
  still hold the previous session's contents.
- Sequential-session test: connect, create resources, disconnect, reset,
  reconnect, and assert the new client sees none of the first session's atoms,
  properties or selections.
- Integration: two consecutive `xterm` sessions against one server with
  `-reset`, confirming the second starts clean and the screen does not retain
  the first session's contents.
- No xts A/B — nothing here draws (the rule is scoped to pixel changes).

## Adjacent gaps, not in scope

- **`KillClient` on another client's resource leaks that client's parked CRTC
  token.** `process_request.rs:22458` calls `process_disconnect` inline rather
  than going through `disconnect_with_pending_cleanup`, so
  `pending.take_client_crtc` / `backend.cancel_crtc_config` never run. Found
  while auditing every client-removal path for the reset trigger; pre-existing
  and unrelated to reset, but it is the only departure that bypasses the
  funnel, so anything else added to that funnel later will miss it too.

- **The composite-overlay claim is not a per-client resource, and that must be
  fixed BEFORE reset ships.** `GetOverlayWindow` increments an anonymous
  backend counter (`core.cow_refcount`); nothing decrements it when the
  claiming client disconnects. `process_disconnect` calls
  `backend.client_disconnected`, which clears the *scene* root-overlay
  contribution (`kms/render/backend.rs:19621`,
  `scene.root_overlay_on_disconnect`) — a different overlay concept with a
  confusingly similar name, and not the COW claim.

  Xorg has no such gap: the claim is a per-client XID resource, so the
  resource system frees it on disconnect (`FreeCompositeClientOverlay`,
  `../xserver/composite/compext.c:88`, calling `compFreeOverlayClient`,
  `compoverlay.c:59`), and the COW is destroyed when the last claim goes.

  The fix, and it is structural rather than reset-specific:
  1. Track COW claim ownership **per client** in core state, so repeated
     `GetOverlayWindow` from one client cannot create unbounded anonymous
     claims.
  2. Share one "release this client's claim" helper between
     `ReleaseOverlayWindow` and both the ordinary and forced disconnect paths.
  3. Reset then inherits the cleanup through `force_destroy_all_clients` with
     no special case — no counting, no looping.
  4. **Final COW teardown failure must be fatal to the reset**, not logged.
     `release_overlay_window`'s `refcount == 1 && scanout_m2.active()` branch
     calls `materialize_direct_shadow_for_unflip()?` and can fail
     (`kms/render/backend.rs:20169`), leaving the refcount untouched by
     design. Continuing past that would start the next XDMCP session with the
     previous user's overlay still held. Either add a reset-specific teardown
     that can complete the unflip safely, or fail/terminate rather than expose
     the next session.

  A bounded decrement loop inside `reset_generation` was written and then
  **rejected** (jos and codex, 2026-09-09): the cap is arbitrary and
  protocol-invalid, and the failure mode is precisely the one reset exists to
  prevent.


- ~~`SetCloseDownMode`/`RetainPermanent`~~ — no longer an adjacent gap. Forced
  destruction of retained and zombie resources is core design (see "Forced
  cleanup"), with its own invariant and test, not something to defer.
- Xorg's `-terminate` delay (`terminateDelay`) is a refinement we can skip
  until someone wants it.
