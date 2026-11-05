# yserver

A modern X11 server written from scratch in Rust. Supports linux/FreeBSD/OpenBSD on DRM/KMS. macOS/windows in nested mode.

Info about the project is in README.md
Current status is in docs/status.md and should be kept up to date
focus is on yserver (KMS)

## Instructions

- it's fine not to use clippy pedantic in this repo but DO use regular clippy
- before committing, run clippy exactly as CI does: `cargo clippy --all-targets -- -D warnings`. CI fails on any warning, and `--all-targets` lints test code too — a crate-scoped or non-`--all-targets` run misses lints in tests (e.g. needless_range_loop) and they only surface on GH.
- use `cargo +nightly fmt` for formatting
- when adding or changing ioctls, watch for libc linuxisms: request-type aliases like `libc::Ioctl` are not portable across all supported targets, and musl/FreeBSD have regressed here before. Keep ioctl request typing/buildability valid on Linux glibc, Linux musl, and FreeBSD.
- design docs (specs) go in docs/superpowers/specs
- impl plans go in docs/superpowers/plans
- work on feature branch for phases
- squash merge when ready (ask confirmation); PRs with multiple commits serving distinct purposes are fast-forward merged instead, so each commit stays bisectable
- Spec compliance is the goal, but if Xorg deviates from spec (unlikely), we need to follow Xorg, clients are tested for 40+ years on Xorg.

## environment

following extra folders should be in your sandbox, ask if not available
- /home/jos/Projects/xserver
- /home/jos/Projects/wlroots
- /home/jos/Projects/xts
- /home/jos/Projects/yserver-packaging
