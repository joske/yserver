# Build features `tcp-transport` / `xdmcp` — implementation plan

Design: [`2026-09-15-build-features-design.md`](../specs/2026-09-15-build-features-design.md).
Read it first; every decision below is argued there and is not repeated.

## Ordering principle

Each step must leave the tree building and passing in **all three**
configurations, not just the default. That is stricter than it sounds: the
whole point of the feature is a build nobody exercises by habit, so a step that
"will be fixed by the next one" is a step that ships broken to whoever builds
minimally in the meantime.

The one ordering constraint that is not obvious: **step 3 must not start until
step 2's stub is complete.** Gating the listener while the core loop is still
half-gated leaves an intermediate commit where `run_core` has some `cfg`s and
some not — the inter-commit state trap, and the exact scattering the design
exists to prevent.

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

## Step 2 — gate the XDMCP modules, add the complete stub

`#[cfg(feature = "xdmcp")]` on `yserver-protocol`'s `xdmcp` module and
`yserver-core`'s `core_loop::xdmcp`. Add the feature-off stub: the ten methods
in the design's table with exactly those inert results, `XDMCP_TOKEN`, and an
`XdmcpOutcome` retaining both `Reset` and `Terminate`.

**Proof.** The acceptance criterion is structural, not behavioural:
`git diff` on `crates/yserver-core/src/core_loop/run.rs` must be **empty**. If
that file changed, the stub is incomplete and the design has already failed.
Beyond that: `--no-default-features` builds, and the 5,291 lines are gone —
check with `cargo llvm-lines` or simply that the modules are absent from the
build plan.

## Step 3 — gate listener creation

`#[cfg(feature = "tcp-transport")]` on the TCP listener bind only.
`Transport::Tcp` stays compiled in every configuration.

**Proof.** `grep -c 'cfg(feature = "tcp-transport")'` over `transport.rs`,
`run.rs` and `auth.rs` is **zero** — the gate belongs at the bind site alone.
A minimal build cannot bind a TCP socket; a default build's existing TCP tests
still pass unchanged.

## Step 4 — startup errors, asserted by message

`-listen tcp` without `tcp-transport` and `-query`/`-broadcast`/`-indirect`
without `xdmcp` fail at startup. The parser stays unconditional, so `-query` in
a minimal build reports the feature, never "unknown option".

**Proof.** Tests assert the **message text**, not merely that startup failed —
a test that only checks for an error passes just as well when the option
becomes unrecognised, which is the failure mode this step exists to prevent.
One test per option group, in the minimal configuration.

## Step 5 — advertise the feature set, and use it

`version::line()` gains the machine-readable suffix in the design's exact
format (`features=[tcp-transport,xdmcp]`, alphabetical, no spaces, present even
when empty). The two feature-dependent recipes build with `--features`
explicitly rather than relying on `default`. `tools/vng-shot.sh --binary` reads
the suffix and fails with an actionable message.

**Proof.** A test pins the format for all three configurations, including the
empty case — the parser in `vng-shot.sh` is the consumer and must not be
handed prose. Then `vng-shot.sh --binary` against a deliberately minimal
binary produces the actionable error rather than a connection timeout.

## Step 6 — test gating, CI, docs

Apply the narrowest-feature rule to every site the design names, including the
`lib.rs:940` exception that stays ungated. Add the three-configuration CI
matrix, each running `clippy --all-targets -- -D warnings` **and**
`test --all-targets`. Note the feature requirement in the man page and
`docs/setup.md`.

**Proof.** The matrix is green. Then the check that matters: temporarily break
something only a minimal build would notice — say, a `cfg`-gated `use` that is
unused without the feature — and confirm CI catches it. A matrix nobody has
seen fail is a matrix nobody knows works.

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
