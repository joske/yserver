# Build features: `tcp-transport` and `xdmcp`

> **Status: not implemented.** Requested by AppleSheeple on PR #148 ("Can this
> be made a crate feature? I would actually prefer disabling this at compile
> time"), agreed by jos as a follow-up to keep #148 from growing further.
> Shape settled with codex 2026-09-15. Precedent: Xorg's `--disable-xdmcp`.
> Implementation plan:
> [`2026-09-15-build-features-plan.md`](../plans/2026-09-15-build-features-plan.md).

## Problem

`dc417cfd` added a UDP XDMCP negotiator and a TCP listener. Both are opt-in at
runtime — with no XDMCP option `build_xdmcp_service` returns `None` and no UDP
socket is opened at all (design invariant 4), and without `-listen tcp` no TCP
socket is bound — but they are unconditionally *compiled in*. A minimal or
security-constrained deployment cannot prove absence by inspection, and pays
for code it will never reach.

## What each feature is worth — measured, not estimated

| feature | excludes | character |
|---|---|---|
| `xdmcp` | **5,291 lines** — `xdmcp/codec.rs` 1423, `xdmcp/state.rs` 2623, `xdmcp/mod.rs` 23, `core_loop/xdmcp.rs` 1222 | real size and attack-surface win |
| `tcp-transport` | a few hundred lines, `std::net` only | removes a **capability**, not weight |

That asymmetry drives the central design decision below. It is the reason the
two features are gated differently, and it should not be forgotten when someone
later proposes "tidying" them into symmetry.

## Design

### Feature graph

Features are **per package**, so a table on `yserver` alone is not enough. All
three manifests need one, and the two library crates must default to nothing:

```toml
# crates/yserver-protocol/Cargo.toml
[features]
default = []          # <- load-bearing
xdmcp   = []

# crates/yserver-core/Cargo.toml
[features]
default = []          # <- load-bearing
xdmcp   = ["yserver-protocol/xdmcp"]

# crates/yserver/Cargo.toml
[features]
default        = ["tcp-transport", "xdmcp"]
tcp-transport  = []
xdmcp          = ["tcp-transport", "yserver-core/xdmcp"]
```

`default = []` on the libraries is what makes the exclusion real. If either
library ever grows a default that includes its own `xdmcp`, then
`cargo build -p yserver --no-default-features` still compiles the core and
protocol XDMCP modules and the 5,291-line saving silently evaporates — a
regression with no symptom, which is the worst kind. **Forwarding belongs in
each package's `[features]` table**, as above; `[workspace.dependencies]` needs
touching only if it is used to control a dependency's default features
(`default-features = false`), not for forwarding.

`xdmcp` depends on `tcp-transport` because XDMCP's entire purpose is the
connect-back: the manager negotiates over UDP and then its session reaches the
display over TCP. XDMCP without a TCP listener is already a startup error on
master; at build level the dependency makes the combination unrepresentable.

This is the workspace's **first** use of Cargo features — there is no
`[features]` table in any of the three manifests today.

### The `Transport` decision — gate the capability, not the type

Keep `Transport::Tcp(TcpStream)` compiled unconditionally. Gate only the
ability to **create a TCP listener**.

Removing the enum variant would fork every `match` on `Transport` and scatter
`cfg` through the run loop, auth and client establishment — for no meaningful
size win, since `TcpStream` is `std` and the variant costs nothing. Gating the
listener yields the property that actually matters: a minimal build **cannot
accept a TCP connection**, and says so at startup.

### What a disabled build does

Fail loudly at startup, never silently ignore:

- `-listen tcp` → `built without tcp-transport`
- `-query`, `-broadcast`, `-indirect` → `built without xdmcp`

Keep the argument **parser** unconditional. `-query` must report "built without
XDMCP support", not "unknown option" — the second sends the operator hunting
for a typo. This also leaves `launch.rs`'s 120 XDMCP references alone.

### What stays unconditional

**Server reset.** It is the X11 generation boundary, has nothing to do with the
network, and `-reset` / `-terminate` policies are useful on a unix-only server.
It arrived in the same PR; that is not a reason to couple it.

Unix transport, auth, and the `fd_passing` dispatch gates are likewise
untouched — the gates are about a transport's *capabilities*, not about
whether TCP was compiled in.

### Advertising the feature set

`yserver::version::line()` feeds both `--version` and the startup banner, so it
is the one place to append the built feature set.

The suffix is a **stable, machine-readable** field, not prose, because
`vng-shot.sh` parses it:

```
features=[tcp-transport,xdmcp]
features=[tcp-transport]
features=[]
```

Alphabetical, comma-separated, no spaces, always present including when empty.
A tool must never have to pattern-match a sentence whose wording is
unconstrained.

## The `cfg` boundary

Naive gating looks enormous — `run.rs` has 95 XDMCP references and `launch.rs`
120. Two moves collapse it to roughly four sites:

1. **A stub `XdmcpService`** when the feature is off — and it must be the
   COMPLETE surface `run_core` uses, or step 2 grows ad-hoc `cfg`s in the core
   loop, which is the exact outcome this design exists to prevent. Measured
   from `run.rs`, that is **ten** methods, with these exact inert results:

   | method | stub returns |
   |---|---|
   | `register(&Registry)` | `Ok(())` — registers nothing |
   | `start(&AuthState, Generation)` | `()` |
   | `restart(&AuthState, Generation)` | `()` |
   | `handle_readable(&AuthState, Generation)` | `()` |
   | `service_timer(Instant, &AuthState, Generation)` | `()` |
   | `next_deadline()` | `None` |
   | `take_outcome()` | `None` |
   | `live_session_client()` | `None` |
   | `note_client_established(..)` | **`false`** — never orphaned |
   | `note_session_client_disconnected(..)` | `()` — **not** `bool` |

   Two of those results are easy to get wrong in opposite directions, so both
   are stated rather than left to symmetry:

   - `note_client_established` is `#[must_use] -> bool` meaning "drop this
     client". A stub guessing `true` would silently disconnect every client in
     a minimal build.
   - `note_session_client_disconnected` looks like its sibling and **is not**:
     it returns `()` (`core_loop/xdmcp.rs:425`). A stub returning `bool` there
     does not type-check.

   `run.rs` also needs two items besides the methods:

   - **`XDMCP_TOKEN`** — the mio token, which must still exist to register
     nothing against.
   - **`XdmcpOutcome`**, which must keep **both** variants, `Reset` and
     `Terminate` (`core_loop/xdmcp.rs:117-123`). `run.rs` matches on both by
     name, so a feature-off enum that dropped either — or became an empty
     enum because the stub never returns one — fails to compile. The stub
     never constructs an outcome; the type still has to admit both.

   `run_core`'s signature never forks, so its 95 references need no `cfg` at
   all.
2. **Keep the parser**, as above — so the 120 references stay put.

Leaving: `pub mod xdmcp` in `yserver-protocol/src/lib.rs`, the module selection
in `core_loop/mod.rs`, `build_xdmcp_service`, the listener bind, and the stub.
`auth.rs`'s 39 references are a `bool` and need nothing.

## CI

Three configurations, because two would not catch the interesting failure:

1. default features;
2. `--no-default-features` — unix-only;
3. `--no-default-features --features tcp-transport` — proves TCP does **not**
   drag XDMCP back in.

Each runs **both** of:

```
cargo clippy --all-targets --no-default-features [--features ...] -- -D warnings
cargo test   --all-targets --no-default-features [--features ...]
```

Building alone leaves `cfg`-specific lint failures dormant for GitHub to find
later, and `--all-targets` is what reaches test code.

### Test gating — the rule, and every site it applies to

The rule: **a test is gated by the narrowest feature whose behaviour it
asserts.** Unix-only tests always run; TCP-only tests take `tcp-transport`;
anything asserting XDMCP codec, state-machine or service behaviour takes
`xdmcp`, even when it lives in a module already gated on `tcp-transport`.

`tcp_tests` is `#[cfg(test)] mod tcp_tests;` (`crates/yserver/src/lib.rs:898`)
today, so it compiles into every test build and starts TCP listeners. It
becomes `#[cfg(all(test, feature = "tcp-transport"))]`.

XDMCP-specific tests need `#[cfg(feature = "xdmcp")]` individually, or they
either fail to compile once `yserver_protocol::xdmcp` is genuinely gated, or
assert behaviour deliberately absent from that build:

- `crates/yserver/src/lib.rs:949` `an_xdmcp_option_opens_a_socket_and_implies_a_reset`
- `crates/yserver/src/lib.rs:1053` `listen_tcp_with_an_xdmcp_option_needs_no_auth_file`
- `crates/yserver-core/src/core_loop/run.rs:4412` `the_xdmcp_socket_is_polled_and_a_reset_re_queries`
- `:4474` `an_xdmcp_terminate_ends_the_core_loop`, `:4590`
  `an_orphaned_xdmcp_client_does_not_reset_the_generation`, and the test
  transport helper at `:4402`
- `crates/yserver/src/tcp_tests.rs:348` `xdmcp_requires_a_tcp_listener_at_startup`
  — inside a `tcp-transport` module but asserting XDMCP behaviour, so it takes
  the **narrower** `xdmcp` gate

`crates/yserver/src/lib.rs:940` `no_xdmcp_option_opens_no_socket` is the
exception: it asserts an ABSENCE that holds in both builds, so it stays
ungated and is worth keeping as coverage in the minimal configuration.

Unix-only tests stay enabled in every configuration — the point is to keep
coverage, not to shed it.

A feature nobody builds rots silently.

## Recipes and docs

Two recipes depend on a feature — `yserver-tcp-hw` on `tcp-transport`,
`yserver-xdmcp-hw` on `xdmcp` — but **neither needs a runtime check**, and
adding one would be dead code. Both begin with `cargo build --release --bin
yserver`, i.e. they build the binary they then run, with default features. They
cannot receive a minimal build.

What they should do instead is build what they need **explicitly**:

```
cargo build --release --features xdmcp --bin yserver
```

so they stay correct if the default set ever changes or a user has configured
otherwise, rather than silently depending on `default`.

`tools/vng-shot.sh --binary` was proposed as a consumer of the feature suffix
and is **not** one: it starts `'$binary' 7` and exports `DISPLAY=:7`, using the
unix socket, and passes neither `-listen tcp` nor any XDMCP option. A minimal
binary is perfectly valid there, so rejecting one would invent a regression and
undermine the feature's purpose. The version suffix remains useful for external
tooling, packagers and diagnostics; vng-shot simply is not a caller.

The man page and `docs/setup.md` describe `-listen tcp` and the XDMCP options
without qualification today; both need a note that they require the
corresponding feature.

## Plan

1. Add the feature tables and workspace forwarding. No `cfg` yet — proves the
   plumbing before any behaviour moves.
2. Gate `yserver-protocol`'s `xdmcp` module and `yserver-core`'s
   `core_loop::xdmcp`, adding the stub service. Build all three CI configs.
3. Gate the TCP listener creation. `Transport::Tcp` stays.
4. Startup errors for `-listen tcp` / `-query` and friends, with tests
   asserting the **message**, not merely the failure.
5. Feature set in `version::line()`; `--features` on the two recipes' own
   builds.
6. CI matrix, man page and `docs/setup.md`.

## Do not

- Remove `Transport::Tcp`. See the trade-off above.
- Gate server reset.
- Make `-query` an unknown option in a minimal build.
- Add a *runtime* switch for any of this — the project's rule is no feature
  kill-switches; this is a build-policy decision and belongs at build time.
