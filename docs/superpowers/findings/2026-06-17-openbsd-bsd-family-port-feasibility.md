# OpenBSD / BSD-family port feasibility (yserver bare-metal, not ynest)

**Date:** 2026-06-17 · **Scope:** architecture assessment, no code changed · **Priority:** LOW — below the macOS/Windows nested port

## Scope note

This is **`yserver` bare-metal KMS work, not `ynest`.** Unlike macOS/Windows (no DRM → nested-windowed port, see `2026-06-17-macos-windows-port-feasibility.md`), OpenBSD *has* DRM/KMS, so the target is the real direct-mode server running on the HW — same shape as the existing FreeBSD support, not a nested backend. Lower priority than the macOS/Windows nice-to-have.

## Verdict

OpenBSD sits **between FreeBSD (nearly free) and macOS/Windows (new backend)**. The split runs down the middle of the stack: the **display half mostly carries over**, the **input/seat/VT half does not**. FreeBSD was cheap because it's API-identical to Linux (libinput, udev, libseat all port to it → ~16 mechanical `cfg(freebsd)` sites reusing the same concrete code). OpenBSD is the first KMS target that's API-*different* — wscons instead of evdev, no udev, no libseat — so inline `cfg` branches stop working and you must introduce a **trait seam**. Feasible, contained, but a real port (~1–1.5 weeks), **gated on the `drm` crate working on OpenBSD** (spike that first).

## Display half — mostly shared

- **DRM/KMS: yes.** OpenBSD ports the Linux DRM drivers in-kernel (`drm(4)`: amdgpu/i915/radeon), so the KMS scanout concept exists; Mesa RADV/etc. exist in ports. Caveats: OpenBSD's DRM tracks a somewhat older Linux snapshot, and the Rust `drm` crate is Linux-shaped — its ioctl ABI is *close* (OpenBSD's DRM is a Linux port) but whether the crate's `cfg` gates and specific ioctls line up is the **#1 unknown**. A non-cooperative `drm` crate balloons the estimate.
- **kqueue: trivial.** `kms/v2/completion_poller.rs` already abstracts epoll/kqueue behind one type; OpenBSD just joins the `cfg(target_os = "freebsd")` kqueue arm. One-line class of change.

## Input / seat / VT half — the wall

FreeBSD has the whole Linux-ish stack; OpenBSD breaks all three pieces:

- **Native input is `wscons`** (`wskbd`/`wsmouse`), not evdev (`/dev/input/event*`). libinput-on-OpenBSD is possible but unidiomatic and still wants evdev. Realistically: write a wscons input path feeding the existing neutral `InputEvent`.
- **No udev.** The `udev` crate won't work; device discovery needs a wscons/static replacement.
- **VT switching is wscons** (`WSDISPLAYIO_*`), not Linux `VT_*`; no logind/seatd → run with DRM master under OpenBSD's privilege model (pledge/unveil, `machdep.allowaperture`/securelevel).

## Where the abstraction layer goes

Today there is **no platform trait** for input/seat/VT — the modules are concrete, single-implementation wrappers, and FreeBSD reused them via inline `cfg` because it's API-identical. OpenBSD is the forcing function to promote them to traits. Insertion points, by readiness:

1. **Input — seam is `input::Context` (`crates/yserver/src/input/context.rs`), already in the right place.** Everything above `Context` already speaks the neutral `InputEvent` (`input_thread.rs` calls `ctx.dispatch() → InputEvent`). Extract a trait at that boundary, e.g.:
   ```
   trait InputSource { fn dispatch(&mut self) -> Vec<InputEvent>; fn fd(&self) -> RawFd; … }
   ```
   Keep the libinput `Context` as one impl; add a `WsconsContext` impl (`WSKBDIO`/`WSMOUSEIO` ioctls). Nothing upstream changes — it already consumes `InputEvent`. Cleanest abstraction point in the codebase.

2. **Session/VT — seam is `seat/mod.rs` + the direct-mode VT path.** Partially exists already: the seat layer has a **libseat-vs-"Direct mode"** split (Direct = no libseat, console-guard VT). OpenBSD is a third variant (or enhanced direct mode) where VT switching uses wscons `WSDISPLAYIO_*` and DRM master is held directly. Scaffolding is there; the VT mechanism in the direct path is Linux-ioctl-concrete and needs a wscons sibling.

3. **DRM (`drm/`) + poll (`completion_poller.rs`) need ~nothing.** Poller already abstracted (OpenBSD joins kqueue arm). `drm/` reused as-is *if* the `drm` crate cooperates; if not, the shim lands here.

## Shared-seam payoff

The `input::Context` → trait extraction is the **same seam the macOS/Windows native backend needs** (it also produces input from a non-libinput source). Promoting `input::Context` from concrete type to trait is a shared prerequisite that unblocks *every* non-libinput target — BSD-wscons and macOS/Windows alike. Do it once.

## NetBSD

Same story as OpenBSD (wscons-based, DRM ported). The BSDs cluster into **"FreeBSD = evdev/libinput like Linux" vs "OpenBSD/NetBSD = wscons"** — that single distinction decides cheap-vs-real.

## Recommended first step

Spike the **`drm` crate on OpenBSD** before anything else — it's the one unknown that decides whether this is "~1–1.5 weeks of wscons input/VT work on a shared display half" or a much bigger job. If the crate works, the rest is the bounded `InputSource` trait + wscons impl + wscons VT path.
