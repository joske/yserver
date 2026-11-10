# GLX DestroyPbuffer and MIT-SHM CreatePixmap defects — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `DestroyPbuffer` frees only the backing pixmap of the pbuffer it destroys, and MIT-SHM `CreatePixmap` no longer
leaks a backend pixmap when the segment is missing or too small.

**Architecture:** Two local fixes in their handlers; no new state.

**Tech Stack:** Rust, crate `yserver-core` (`core_loop/process_request/{glx,xshm}.rs`).

**Spec:** this plan (two pre-existing defects found while planning access control PR 3c-i, #357; approved by the user
2026-11-10). Reference (MIT): the Xorg checkout at `/data/Projects/xorg-drivers/xserver` (master, 26.1.99.1); the GLVND
front door cited below (`glx/vndcmds.c` `CommonMakeCurrent`) has the same shape since xserver 1.20.

**Not a defect (checked):** GLX `MakeCurrent` with context 0 and non-zero drawables. Xorg's `glxcmds.c:588-590`
`BadMatch` is not reachable from the request: the GLVND front door (`glx/vndcmds.c` `CommonMakeCurrent`) answers
Success with tag 0 when there is no old and no new context (`:231-233`), and otherwise loses the old context with
`None` drawables (`:164-166`) and answers tag 0 (`:265-271`). yserver already answers the same.

## Global Constraints

- Every other request and error is byte-identical to today; the order of validity errors in `CreatePixmap` does not
  change.
- Access control checks stay where #357 put them: `CreatePixmap` asks `CREATE` before the backend allocation and the
  segment `READ` after the segment's own validation; `DestroyPbuffer` asks `DESTROY` for a resolving GLX drawable.
- Reuse first; no new `YSERVER_*` variable; no `cfg(test)` in production bodies.
- Tests go through `process_request` with `RecordingBackend` (which records `create_pixmap` / `free_pixmap`). Every test
  names the mutation that fails it; every mutation is run. Suites under CPU load. Test files stay under 6000 lines.
- Licensing: Xorg is MIT and may be read; no code copied.

## Review Focus

1. `DestroyPbuffer` on a pbuffer still frees its backing pixmap (core entry and backend object), as today.
2. `DestroyPbuffer`, `DestroyPixmap` and `DestroyWindow` naming an id that has no GLX record, or a GLX record of another
   kind, never free a core pixmap.
3. `CreatePixmap` with a missing segment, an undersized segment, and a valid one: exactly one backend allocation only in
   the valid case; the no-policy errors are today's.

---

### Task 1: DestroyPbuffer frees only its own backing pixmap

**Files:** `core_loop/process_request/glx.rs` (the `DELETE_WINDOW | DESTROY_PIXMAP | DESTROY_PBUFFER` arm); tests in
the existing GLX test file under `process_request/tests` (or `access_glx.rs` if that is where pbuffer fixtures live).

**Invariant:** the core pixmap under the XID is freed only when the request is `DESTROY_PBUFFER` **and** the GLX record
removed by this request existed and was of kind `GlxDrawableKind::Pbuffer` (the only case in which `CreatePbuffer`
created that pixmap). Today it is freed for any `DESTROY_PBUFFER`, whatever the record; `DestroyPixmap` and
`DestroyWindow` never free it, and still do not (sol r1 M-1).

- [ ] **Step 1: Failing tests**
  - `destroy_pbuffer_without_pbuffer_keeps_foreign_pixmap`: client A creates a core pixmap P; client B sends
    DestroyPbuffer(P): A's pixmap entry and backend object remain (no `free_pixmap` recorded). Mutation: free on any
    `DESTROY_PBUFFER` (today).
  - `destroy_pbuffer_frees_its_backing_pixmap`: CreatePbuffer then DestroyPbuffer: the backing pixmap is freed as today.
    Mutation: never free.
  - `destroy_pbuffer_on_glx_pixmap_record_keeps_pixmap`: DestroyPbuffer naming a GLX pixmap's id: no core pixmap is
    freed. Mutation: free when any GLX record existed.
  - `destroy_pixmap_or_window_on_pbuffer_keeps_backing_pixmap`: CreatePbuffer P, then DestroyPixmap(P) and, in a second
    fixture, DestroyWindow(P): P's core pixmap and backend object survive, no `free_pixmap` recorded. Mutation: drop the
    opcode condition (free whenever the removed record is a Pbuffer).
- [ ] **Steps 2–4:** run, fail; implement; run, pass, whole suite unmodified.
- [ ] **Step 5: Commit** `fix(glx): DestroyPbuffer frees only its own backing pixmap`.

### Task 2: MIT-SHM CreatePixmap allocates after the segment checks

**Files:** `core_loop/process_request/xshm.rs` (`CreatePixmap`); tests beside the existing SHM tests
(`tests/access_shm.rs` holds the #357 rows).

**Invariant:** the backend allocation (`backend.create_pixmap`) happens after the segment's existence check, its
`READ` access check and the offset/size bounds check, and before the upload; so no validity or denial return can leave
an allocation behind. The `CREATE` access check stays before the allocation. The rollback added in #357 for a denied
segment becomes unnecessary and is removed (with its failing-free logging), since nothing is allocated at that point.

- [ ] **Step 1: Failing tests**
  - `shm_create_pixmap_missing_segment_allocates_nothing` and `shm_create_pixmap_undersized_segment_allocates_nothing`:
    today's `BadValue` with today's value, no `create_pixmap` recorded, no pixmap registered. Mutation: allocate before
    the segment check (today).
  - `shm_create_pixmap_valid_allocates_once`: one `create_pixmap`, one upload, the pixmap registered as today.
  - the #357 rows (segment denial) keep passing; their expectation of an allocation-then-free becomes "no allocation".
- [ ] **Steps 2–4:** as Task 1.
- [ ] **Step 5: Commit** `fix(shm): CreatePixmap allocates after the segment checks`.

### Task 3: Status and gate

- [ ] **Step 1:** `docs/status.md`: one entry at the top for both fixes, and the MakeCurrent context-0 finding.
- [ ] **Step 2: Gate:** `cargo +nightly fmt --all`; `cargo build --bin yserver`; `cargo test --workspace --locked
  --features xdmcp`; the ignored suite `--workspace --locked --features xdmcp -- --ignored` (lavapipe env); `cargo
  clippy --all-targets -- -D warnings` and `--workspace --all-targets --locked --features xdmcp -- -D warnings`;
  FreeBSD clippy; `tools/check-layout.sh`; `just man`; the GLX and SHM test modules 5 runs under 16 busy loops.
- [ ] **Step 3: Commit** `docs(status): GLX DestroyPbuffer and SHM CreatePixmap fixes`.
