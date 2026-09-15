# Build features `tcp-transport` / `xdmcp` — implementation plan

Design: [`2026-09-15-build-features-design.md`](../specs/2026-09-15-build-features-design.md).
Read it first; every decision below is argued there and is not repeated.

## Ordering principle

Each step must leave the tree building and passing in **all three**
configurations, not just the default. That is stricter than it sounds: the
whole point of the feature is a build nobody exercises by habit, so a step that
"will be fixed by the next one" is a step that ships broken to whoever builds
minimally in the meantime.

This is why steps 2 and 3 are each **one commit** rather than a gating step
followed by a validation step. Splitting them produces an intermediate commit
where a minimal binary accepts `-query` or `-listen tcp` and silently comes up
without it — the inter-commit state trap, and a direct violation of the
design's "fail loudly, never silently ignore" rule. Each step gates a feature
and teaches the binary to refuse that feature's options in the same breath.

## Prerequisites

- `dc417cfd` on master (XDMCP, TCP transport, server reset).
- No `[features]` table exists in any of the three manifests today. This is the
  workspace's first use of Cargo features, so step 1 is plumbing only.

## Step 1 — the feature tables, and nothing else

Add `[features]` to all three manifests exactly as the design specifies:
`default = []` on `yserver-protocol` and `yserver-core`, the real defaults on
`yserver`, forwarding via each package's own table. No `#[cfg]` anywhere yet,
so nothing changes behaviourally in any configuration.

**Proof.** All three CI configurations build and test green — identically to
master, since no code is conditional yet. Then
`cargo tree -f '{p} {f}' --no-default-features` shows `yserver-core` and
`yserver-protocol` with no features active: that is the assertion that the
later exclusion is real, and it is cheapest to make now while nothing else
could explain a failure.

## Step 2 — gate XDMCP: modules, stub, entry point, tests, and its startup error

One commit, because the pieces are not separable without leaving a build that
either does not compile or silently ignores an option.

- `#[cfg(feature = "xdmcp")]` on `yserver-protocol`'s `xdmcp` module and
  `yserver-core`'s `core_loop::xdmcp`.
- The feature-off stub: the ten methods in the design's table with exactly
  those inert results, `XDMCP_TOKEN`, and an `XdmcpOutcome` keeping both
  `Reset` and `Terminate`.
- **The rejection goes in `validate_tcp_startup`**, under
  `#[cfg(not(feature = "xdmcp"))]`, not in `build_xdmcp_service`. This is the
  load-bearing detail. `validate_tcp_startup` runs at `lib.rs:195`, before
  hardware or sockets; `build_xdmcp_service` runs at `:498`, *after*
  `bind_client_listeners` and after KMS initialisation. Rejecting there would
  mean `-query … -listen tcp` on a no-XDMCP binary initialises the GPU, binds
  a TCP listener, and only then fails — briefly opening TCP on a binary that
  cannot serve XDMCP at all, and recreating precisely the startup-order defect
  `dc417cfd` fixed.
- **`build_xdmcp_service` both ways.** It unconditionally does
  `use yserver_core::core_loop::{XdmcpMode, XdmcpService, XdmcpSetup};`
  (`crates/yserver/src/lib.rs:129`), so gating the core module without gating
  this breaks the minimal build outright. The feature-off version returns
  `Ok(None)` for no XDMCP option; it may also error defensively for an option,
  but it is the second line of defence, never the guard.
- **The XDMCP test gating**, every site the design names. Those tests reference
  the now-gated path and will not compile otherwise.

**Proof.** The acceptance criterion is structural: `git diff` on
`crates/yserver-core/src/core_loop/run.rs` must be **empty**. If that file
changed, the stub is incomplete and the design has already failed. Then all
three configurations build and test.

The behavioural test is **`-query <host> -listen tcp` in the
`--no-default-features --features tcp-transport` configuration**: it must fail
in `validate_tcp_startup`, before any listener is bound. That combination is
the one that distinguishes an early guard from a late one — with `-listen tcp`
alone valid in that build, a late rejection would have already bound the
socket.

## Step 3 — gate the TCP listener, its tests, and its startup error

One commit, for the same reason. Gating the bind without the validation leaves
a minimal binary that accepts `-listen tcp` and comes up unix-only without
saying so, which is precisely the "fail loudly, never silently ignore" rule the
design turns on.

- `#[cfg(feature = "tcp-transport")]` on listener creation only.
  `Transport::Tcp` stays compiled in every configuration.
- `tcp_tests` becomes `#[cfg(all(test, feature = "tcp-transport"))]`.
- `-listen tcp` without the feature fails at startup.

**Proof.** `grep -c 'cfg(feature = "tcp-transport")'` over `transport.rs`,
`run.rs` and `auth.rs` is **zero** — the gate belongs at the bind site alone. A
minimal build cannot bind a TCP socket and says so; a default build's TCP tests
pass unchanged.

Both startup errors are asserted by **message text**, not merely by failing. A
test that checks only for an error passes equally well when the option becomes
unrecognised, which is the failure these steps exist to prevent.

## Step 4 — advertise the feature set

`version::line()` gains the machine-readable suffix in the design's exact
format (`features=[tcp-transport,xdmcp]`, alphabetical, no spaces, present even
when empty). The two feature-dependent recipes build with `--features`
explicitly rather than relying on `default`.

**Proof.** A test pins the format in all three configurations, including the
empty case. Note `tools/vng-shot.sh` is **not** a consumer — see the design; it
runs unix-only and a minimal binary is valid there.

## Step 5 — CI matrix and documentation

The three-configuration matrix, each running
`clippy --all-targets -- -D warnings` **and** `test --all-targets`.

Documentation lands **here, with the code** — not earlier. A man page that
describes a feature flag before the flag exists is simply wrong for everyone
who reads it in the meantime, and there is no minimal build for it to
describe yet.

`docs/man/yserver.1.scd` mentions build options **nowhere** today, so this
introduces the idea to it. Three touch points:

- `-listen` / `-nolisten` (`:55`) — requires `tcp-transport`
- the XDMCP options (`:99` `-query`/`-indirect`/`-broadcast`, plus `-port`,
  `-from`, `-class`, `-displayID`, `-once`) — require `xdmcp`
- `--version` — now prints the built feature set, which is how a reader checks
  which they have

Placement is a decision, not a discovery: either repeat a note on each of the
seven-odd options, or state it once under `DESCRIPTION` (some options require
build features; `--version` lists them) and flag the options themselves
briefly. The existing `XDMCP AND TRUST` section (`:279`) is the natural home
for the XDMCP half. Prose is jos's; this plan only fixes where it goes.

**Proof.** The matrix is green. Then the check that matters: temporarily break
something only a minimal build would notice — a `cfg`-gated `use` left unused
without the feature — and confirm CI catches it. A matrix nobody has seen fail
is a matrix nobody knows works.

## Hazards

- **A passing default build proves nothing here.** Every step's risk lives in
  the configurations habit does not exercise.
- **The stub is the whole design.** If it drifts from the live API — a method
  added to `XdmcpService` later without a matching stub method — the minimal
  build breaks and the default build does not notice. CI config 2 is what
  catches that, which is why it must test and not merely build.
- **Do not gate server reset**, and do not remove `Transport::Tcp`. Both are
  argued in the design; both would look like tidying.
- **`cargo test --no-default-features` must reach test code.** Without
  `--all-targets`, gated test modules are not compiled and the gating is
  unverified.
