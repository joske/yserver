# Access control PR 3c-i — maintainer review round 1

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Address joske's review of #357: an arbitrary-resource operand resolves over every resource kind, a GLX
window is found under its X window id, the release form of MakeCurrent checks nothing, DestroyPbuffer's derived pixmap
check goes, and the spec records the remaining differences from Xorg.

**Architecture:** Each fix stays in the helper or handler that owns it. No new `ResourceKind`, no new `AccessMode`.

**Tech Stack:** Rust; `yserver-core` (`core_loop::process_request`, `server.rs` resolvers).

**Spec:** `docs/superpowers/specs/2026-10-30-access-control-layer-design.md`; base plan
`docs/superpowers/plans/2026-11-09-access-control-pr3c-i.md`. Reference (MIT): Xorg 21.1.24 at
`/data/Projects/xorg-drivers/xserver`, XLibre at `/data/Projects/xlibre`.

## Global Constraints

- Everything in the base plan's Global Constraints binds every task (shared test policy path in `tests/extensions.rs`;
  a DENY and an ASK row per site; checks right after each operand's own validation; `SERVER_OWNER` unchecked; no new
  validity errors; reuse first; no `cfg(test)` in production bodies; every new test names the mutation that fails it).
- With no policy installed, nothing a client can observe changes. In particular `KillClient`'s `BadValue` keeps
  `resource_owner`'s scope (`resources/cleanup.rs`), and every request answers exactly as today.
- `tools/check-layout.sh` must pass (test files at most 6000 lines).

## Review Focus

1. An id that resolves to nothing, through the wide resolver, is not asked and keeps today's bytes.
2. An id that several stores could hold: the resolver's kind order is deterministic and documented.
3. The GLX alias applies only to a core window that has a GLX drawable of kind Window recorded for it; a GLX id still
   resolves as today.
4. MakeCurrent / MakeContextCurrent with context 0: no check at all, whatever the drawables are; the reply is today's.

---

### Task 1: Arbitrary-resource operands resolve over every resource kind

**What joske found:** SYNC `SetPriority` / `GetPriority` resolve their id with `validate_core_resource_if_present`
(`process_request.rs` `core_resource_kind_for_access`), which only knows Window, Pixmap, Gc, Font, Cursor, Picture,
GlyphSet and Dri3Syncobj. Xorg looks the id up with `dixLookupClient` (RC_ANY), so any resource names its client. With a
deny policy, both requests naming an existing SYNC counter succeed without the policy being asked.

**Same pattern, same fix:** `KillClient` (`process_request/misc.rs`), `XISetClientPointer` and `XIGetClientPointer`
(`xi/dispatch/devices.rs`) use the same helper, and Xorg uses `dixLookupClient` there too (`dix/dispatch.c:3347`,
`Xi/xisetclientpointer.c:85`, `Xi/xigetclientpointer.c:70`). They switch with it. Only their access check changes;
`KillClient`'s validity (`BadValue` from `resource_owner`) is untouched.

**Files:** `core_loop/process_request.rs` (the arbitrary-resource helper), its four callers, tests in
`tests/access_sync.rs`, `tests/access_input.rs`, and the KillClient rows of `tests/access_resources.rs`.

**Invariants:** the arbitrary-resource helper tries every `ResourceKind` that `ServerState::access_resource_owner`
resolves, in one documented order (the current eight core kinds first, in today's order, then the extension kinds),
and asks for the first kind that resolves; an id that resolves to no kind is not asked. The DRI3 drawable lookups that
call `core_resource_kind_for_access` for a Window/Pixmap keep their narrow meaning (rename or split the helper so the
two uses stay distinct).

- [ ] **Step 1: Failing tests**
  - `set_get_priority_extension_resource_is_asked` (access_sync.rs): DENY rows for SetPriority and GetPriority naming
    an existing SYNC counter and a GLX context (policy error, no state change), and ASK rows recording the extension
    kind. Mutation: keep the eight-kind list.
  - `kill_client_extension_resource_is_asked` and `xi_client_pointer_extension_resource_is_asked`: the same for a SHM
    segment or colormap; KillClient's no-policy row on that id keeps today's reply (resource_owner scope).
  - unresolved-id rows stay as they are.
- [ ] **Steps 2–4:** run, fail; implement; run, pass, whole suite unmodified.
- [ ] **Step 5: Commit** `fix(core): access control PR 3c-i review: arbitrary-resource operands resolve over every kind`.

### Task 2: GLX window alias, MakeCurrent release form, DestroyPbuffer's pixmap

**Files:** `core_loop/process_request/glx.rs`; tests in `tests/access_glx.rs`; spec row removal in Task 3.

- **GLX window under its X window id.** Xorg registers a GLXWindow also under the X window's id
  (`glx/glxcmds.c:1192-1195`), so MakeCurrent / MakeContextCurrent / SwapBuffers on that X id ask `WRITE` on the GLX
  drawable (`:491`). yserver keys `glx_drawables` by the GLX id only and asks `GET_ATTR` on the core window. In the
  drawable fallback, when the id is a core window and a `GlxDrawable` of kind Window has `x_drawable` equal to it, ask
  `WRITE` on that GLX drawable (target id = the GLX drawable's id, whose owner `AccessContext::owner` resolves) instead
  of `GET_ATTR` on the window. Other ids keep today's checks.
- **MakeCurrent release form.** With context 0 (MakeCurrent and MakeContextCurrent), nothing is checked: Xorg returns
  `BadMatch` before any lookup when the context is 0 and a drawable is not (`glx/glxcmds.c:588-590`), and the plain
  release looks nothing up. yserver keeps today's reply; the missing `BadMatch` is a pre-existing defect for the
  separate PR.
- **DestroyPbuffer's derived pixmap check goes.** Xorg frees every resource under that XID without asking
  (`FreeResource`, `dix/resource.c`); once the separate fix frees only the pbuffer's own backing pixmap, that pixmap is
  always the client's own. Remove the derived check and its tests; the GLX drawable's own `DESTROY` stays.

- [ ] **Step 1: Failing tests**
  - `make_current_glx_window_by_x_id_asks_write_on_glx_drawable` (and SwapBuffers): ASK row records the GLX drawable
    with `WRITE`; a deny on it answers the policy error. Mutation: keep the window `GET_ATTR`.
  - `make_current_context_zero_checks_nothing`: context 0 with non-zero draw/read and a deny-all policy → today's
    reply, no recorded check. Mutation: run the drawable checks for context 0.
  - remove `destroy_pbuffer_denied_core_pixmap_is_kept` and the derived-pixmap ASK rows; keep the GLX drawable
    DESTROY rows.
- [ ] **Steps 2–4** as Task 1. **Step 5: Commit** `fix(core): access control PR 3c-i review: GLX window alias, release form, DestroyPbuffer`.

### Task 3: Spec notes, status and gate

- [ ] **Step 1:** spec:
  - remove the denial-table row for DestroyPbuffer's same-XID pixmap;
  - "What a policy can see": arbitrary-resource operands (SYNC priority, KillClient, XI client pointer) resolve over
    every resource kind; a GLX window is also found under its X window id;
  - note the remaining differences from Xorg, each policy-visible only: SetCounter / ChangeCounter / DestroyCounter on
    a system counter answer `BadAccess` before the policy is asked (Xorg asks first; the client sees the same error);
    Present `SelectInput` with mask 0 and a new event id asks `CREATE` for a selection Xorg would not allocate.
- [ ] **Step 2:** `docs/status.md`: amend the 3c-i entry.
- [ ] **Step 3: Gate** as the base plan's Task 9.
- [ ] **Step 4: Commit** `docs: access control PR 3c-i review round 1 spec notes`.
