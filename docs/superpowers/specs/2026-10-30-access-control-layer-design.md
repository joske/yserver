# Access control layer

Status: draft, revised after the maintainer's answer on #320. Nothing is
implemented.

## Why

Discussion #123 asks how to manage permissions between X clients. The
maintainer sets the direction: an access control layer means checks on what
connected clients may do, the near use case is SECURITY, and the eventual goal
is application isolation, with groups of cooperating clients, controlled access
between groups and explicit privileges for the WM, compositor and trusted
session services. Permissions such as clipboard access, input capture and input
injection will eventually be granted by a trusted session broker, with yserver
enforcing the grants.

Today a connected client can read another client's window, listen to the
keyboard, inject input and read properties and selections. The checks that
exist are setup authorization, a transport gate on three extensions, and the
two access decisions #321 added for the host list. Nothing in yserver can
answer "may this client do this operation on this object?", so any future rule
would have to add its own scattered checks, and one missed site is a hole.

This spec defines the layer and a series of smaller PRs shaped around one
concrete use case: untrusted SECURITY cookies, as `ssh -X` requests them. The
SECURITY policy's rules get their own spec; this one fixes what they rest on.

## Goals

- One interface that answers that question, consulted on behalf of a client at
  every operation SECURITY and Xnamespace both control.
- Several policies apply together, and any of them can deny.
- A policy can see what it needs to decide without yserver growing a new check
  signature for each policy.
- Checks never block, and a grant can be scoped and revoked later.
- With no policy installed, every byte a client observes stays the same,
  including error precedence.
- Tests prove each check site really denies, using an installed test policy
  through the production dispatcher.

## Non-goals

- The session broker, prompting, polkit and portals. Only the properties in
  "Checks never block" and "Scoped, revocable grants" are in scope.
- Client isolation and Xnamespace policies. The interface must not block them.
- SECURITY's rule tables, which belong to that policy's spec.
- **Selection checks and mediation. Clipboard isolation stays explicitly
  unfinished after this series:** an untrusted client is not protected from, or
  prevented from, selection transfers by anything specified here. Mediating them
  will need an interface other than plain checks.
- Rewriting operations: property filters, virtual roots, renamed selections,
  image censoring, `forcedBG`. Each needs its own interface later.
- Who may connect: #321 owns that.

## Reference model

XLibre (Xorg-derived) has XACE plus separate callback lists. These are the ten
hooks both SECURITY and Xnamespace register, which set this spec's scope:

| Hook | Decides |
|---|---|
| Resource | a client's access to a window, pixmap, GC, etc.; also creation |
| Property | access to a window property |
| Send | a client sending an event to a window |
| Receive | a client receiving an event |
| Extension access | whether an extension is visible in discovery |
| Extension dispatch | whether an extension request may run |
| Server | server-global operations (font path, grabs, access control, etc.) |
| Client | access to another client (KillClient, X-Resource) |
| Device | read, change, focus, grab, use of an input device |
| Client state | notification of connection lifecycle |

The XACE selection hook and the screen and screensaver hooks are left out:
neither consumer registers them. Xnamespace handles selections through a
filter, which is a non-goal here, as is any selection check in this series.

## Design

### The interface

A new module `crates/yserver-core/src/access.rs` defines a trait with one
method per hook, each defaulting to allow, and an answer type:

```rust
pub enum Decision { Allow, Deny(DenyError) }
```

`DenyError` is the X error the policy would like reported, such as `BadAccess`
or the `BadValue` Xnamespace uses to conceal a client. It only says which error;
the site decides whether a denial becomes an error at all.

### Several policies at once

The server holds an ordered list of policies, empty by default, with an ordinary
installer method. A check asks them in order and stops at the first `Deny`,
whose error it reports. `Allow` means only "this policy does not object": no
policy can override another's denial, and there is no "grant" answer. Order
therefore changes only which error is reported, never whether the operation is
allowed. There is no environment variable and no `cfg(test)` in production bodies.

Check helpers return `Allow` without a dynamic call when the list is empty, the
same null-list shortcut XLibre has, so the hot paths pay a branch.

Lifecycle notifications go to every policy in order, including policies that
denied something.

### Surviving a reset

The list lives beside the host list, in `ServerState.host_access`, whose handle
`reset.rs` already carries into the fresh state with the new generation. There is
no second carry-over path. `run_core` assigns that handle at startup
(`run.rs:2127`), and that assignment must keep policies installed before it.

### Checks never block

A check is a synchronous call that answers immediately from state the policy
already holds. It performs no I/O and waits on nothing. A grant that needs the
user is denied for now; asking is the broker's job, outside this call, and the
client retries or observes the grant later. The core loop runs these calls
inline, so a policy that waits stalls every client: this is a contract on policy
authors that the signature expresses, returning a `Decision` and never a future
or a pending state.

### Scoped, revocable grants

The layer stores no grants and no decisions. Each check reaches the policies, so
a grant withdrawn from a policy's own state applies from the next check. No site
caches an `Allow` beyond the operation that asked, and the receive check runs at
each delivery, so a revoked client stops receiving.

Scope is the policy's business: every check says which client, object and mode
it concerns, which is what a grant needs to be scoped by. A revocation does not
undo what a past check allowed, such as an active grab or an existing selection
of events on a window; a future broker that needs that gets its own operation.

### What a policy can see

Every check method receives a read-only context along with the facts in the table
below. Policies need ownership, root identity and atom names to decide at all,
and they must not hold references into server state between calls.

| Through the context | Source today |
|---|---|
| Owner of a checked object, including retained, colormap and extension resources | a resolver for this context, below |
| Whether a window is a root window | screen state |
| Atom name | atom table |
| A client's peer identity | `ClientState.peer` (#321) |
| How a client was admitted | new, below |
| Opcodes of the request being run, if any | dispatcher |
| Input focus, time of last user input | server state, added only with the policy that reads them |

The context is built only when the list is not empty. Handlers often hold a
mutable borrow of their client when they reach a check, as `handle_list_hosts`
does, so the plan must show that each site can lend server state read-only; a
site that cannot takes the facts it needs out first.

**Ownership.** `resource_owner` does not cover colormaps or extension stores such
as SYNC, and `KillClient` relies on what it returns: broadening it would change
`KillClient`'s `BadValue` with no policy installed. It stays as it is. The context
gets its own resolver, reusing the owner each store already keeps, and the plan
lists one row per checked resource type.

**Objects being created.** The new id has no entry yet, so the resource check for
a creation passes the id, the type and a creation record: its parent window or
drawable where the request names one, and the creating client as owner.

**Which cookie admitted a client.** Not recorded today: `AuthState::check` answers
only allow or reject, in the setup thread. It will also return an admission
record, carried beside `peer` to `ClientState` and shown to the connected
notification:

```rust
pub enum Admission {
    Cookie { credential: CredentialId, source: CredentialSource },
    HostAccess,
    Internal,
}
```

`CredentialId` is stable and opaque for the credential's life, and identifies which
cookie matched when several are loaded; a policy can assign a client to a group by
it and never sees the secret. `CredentialSource` starts as file or session, and
SECURITY adds generated. A client whose cookie failed and whom the host list then
admitted is `HostAccess`. The record never changes after setup.


The modes are the subset of XLibre's `Dix*Access` that the sites use, as a small
bit set. Exact signatures belong in the plan, but each hook must receive at
least this, because the two future policies read it. Every check made while
running a request also receives that request's major and minor opcode, since
Xnamespace tells apart requests that share a mode:

| Hook | Information given to the policy |
|---|---|
| Resource | client, object id and type, mode |
| Property | client, window, property atom, mode |
| Send | sender, target window at this step of propagation, optional device, the event batch |
| Receive | recipient, delivery window if there is one, the event batch |
| Extension access | client, extension |
| Extension dispatch | client (the opcodes are the question) |
| Server | client, mode (`may_manage_access` and `may_read_access` ask this) |
| Client | client, target client, mode |
| Device | client, device, mode |

### A denial is interpreted by its site

`Deny` does not always mean an X error. As in the reference, each site decides:

| Site | On deny |
|---|---|
| Resource, property, device, client, server on a whole request | the policy's `DenyError` |
| A check per returned entry, such as `XResQueryClients` | the entry is left out and the reply succeeds |
| Core `SendEvent` | no delivery, and the request succeeds |
| Receive | no delivery and no error |
| Extension access | the extension is reported as absent |
| `QueryKeymap` and similar reads | the zeroed reply XLibre gives |

A check authorizes either a whole request or one entry of a list, and the plan
records which for each site. Rewriting is out of scope, but because sites own
their denial, adding a filter or censor beside a check later does not change
this interface.

### Who is checked

Checks run only for work done on behalf of a client request. Hit testing,
ancestor walks, clipping, damage propagation, reset, disconnect, deferred
completion and rootless window management run as the server and are not
checked. The internal clipboard bridge is a real client and goes through the
same checks; a policy decides how to treat it.

Server-generated events have no sender, so the send check does not run for
them. Their recipients do go through the receive check.

### Where the checks go

**Resource.** Not in `ResourceTable`'s getters: `window()` alone has 268
production callers, most of them server housekeeping, and the mutators and
backend-only resources bypass the getters. The check goes at each request's
operand validation, using the existing convergence points where they exist
(`validate_drawable_and_gc`, `validate_drawable_only`, `validate_gc_only`, the
SYNC helpers, `verify_pictures!`), and in the handler otherwise. Operand
validation order is observable, so checks follow the existing order and never
consolidate it.

Operands are not the only targets. When a request acts on objects it derives,
each one gets its own check with the requesting client, as `DestroySubwindows`
does for every child. Traversing to find them stays unchecked server work.

### Denial with several targets

Whether earlier work survives a later denial is client-visible, so it follows
the reference, per request. XLibre's `DestroySubwindows` checks each child immediately before
destroying it and stops at the first denial, so children already destroyed stay
destroyed, their notifications were sent, and the error is reported. A policy
denial therefore does not imply "state unchanged". The plan lists every request
with several targets and records its Xorg ordering.

yserver's `DestroySubwindows` differs in structure: it prepares every child,
unmaps them in bulk, then destroys them, so a check in its last loop would come
after denied children were unmapped. Moving everyone to one-by-one destruction
would change notification order with no policy installed. So with an empty list
the bulk path stays byte-identical, and with any policy installed the request
destroys child by child through the path `DestroyWindow` uses, checking each. A request that checks all its
operands before acting, or that has one target, does leave the state unchanged.

**Receive.** No single function has the client, the event and the window: the
common writer only has bytes. The set that covers delivery is the two fanout
writers, `write_to_client`, the four RANDR emitters and `send_to_recorder`.
Their callers state what the event is about through a new required parameter,
so the compiler finds every call site:

```rust
pub enum EventSubject { Window(ResourceId), Drawable(ResourceId), Device(u16), Global }
```

The callers also make the event available to the policy, which the encoding
closure hides today; the plan decides how, without encoding for a client that
will be denied.

The check follows what a notification means, not how it is buffered. A selected
event appended to the originator's reply, such as `DeviceMappingNotify` in XI,
goes through the receive check, and a denial drops that event and keeps the
reply and the other events. Only a direct result of the request, such as
`GraphicsExpose`, is exempt, since its resource checks already cover it.

**Send.** `SendEvent` and XI `SendExtensionEvent`, at the request. A propagated
send is checked at each candidate window, so its recipient resolver must keep
the window it resolved.

**XTEST.** `FakeInput` has no target window, so it is not a send. It is
authorized at admission by the extension dispatch check and a device write
check, as in XLibre, because the input path keeps only the device and loses the
client.

**Property.** The window property handlers use the property hook. XI1 and XI2
device properties use the device hook, and RANDR output properties the resource
hook, as in XLibre. Requests that read and delete, such as `GetProperty` with
`delete`, ask for both modes. Server writes through `change_property_as_server`
are not checked.

**Extension dispatch and access.** Dispatch has one site in `process_request`,
and access is in `QueryExtension` and `ListExtensions`.

**Server.** `may_manage_access` and `may_read_access` in `process_request/misc.rs`
(#321) ask the server hook after their existing rule, so the error stays
`BadAccess` where they already answer it and `is_local` still applies with no
policy. Other server sites follow XLibre's call sites.

**Client, device.** Follow XLibre's call sites, mapped to yserver's handlers in
the plan.

**Client state.** The policy is told three things, in this order of a client's
life:

1. Connected, before its first request is dispatched, with its `ClientState.peer`
   and its admission record.
2. Connection gone, before its `ClientState` is removed. Its retained resources
   keep their original owner, so the owner id can still appear in later checks.
3. Owner gone, when its last retained resources have been destroyed.

A reset keeps the same policy objects and tells them the generation ended, so
they drop what they hold per client.

### The existing transport gate

`reject_non_local_extension_request` is not a permission policy. Its rules come
from the transport, it must apply with no policy installed, and it answers with
a specific error per extension and minor. It stays where it is, unchanged, and
the dispatch check runs right after it.

## The series

Each PR lands with its own gate and leaves behaviour unchanged for every client
until the last one.

| PR | Content | Reachable in production |
|---|---|---|
| 1 | Interface, policy list, reset carry-over, context skeleton, lifecycle, admission record, server hook at the #321 sites | Sites run, list empty |
| 2 | Extension access and dispatch checks | Same |
| 3 | Resource and property checks, including creation and several-target requests | Same |
| 4 | Send and receive checks, with `EventSubject` | Same |
| 5 | Device and client checks, remaining server sites | Same |
| 6 | SECURITY in one step: protocol, generated credentials, rules, dispatch and discovery | First denying policy |

Nothing of SECURITY is wired before PR 6: hiding it from discovery alone would not
be enough, since dispatch selects handlers by opcode. Until then its requests
behave as they do today and no generated credential can be issued or admitted, so
no client can hold an untrusted cookie while only part of its isolation is
enforced. The deny branches run only in tests until then. PR 6 can split further
if its size calls for it, as long as dispatch and credential issue land last.

## Testing

Per PR, for the sites that PR adds:

- **Bytes unchanged:** the existing suite passes unmodified, including the
  transport gate's and #321's tests, with an empty list.
- **Every site denies:** a table-driven test through `process_request`, one row
  per request or event family, installs a denying policy and asserts the site's
  result from the table above. Single-target requests also assert nothing reached
  the backend and the state is unchanged.
- **Every site asks:** a recording policy asserts each request presents the
  expected client, object, mode and context.
- **Several policies:** with allow then deny and deny then allow, the operation is
  denied both ways, the first denier's error is reported, and a policy after a
  denier is not asked. Lifecycle notifications reach all of them.
- **Several targets:** with an empty list `DestroySubwindows` emits today's bytes.
  With a policy, `DestroySubwindows` with a denied child destroys the children
  below it in stacking order, as Xorg walks from the bottom, keeps the denied
  child and those above it, and emits the same notifications in the same order. A propagated send stops at the denied window; a denied list entry is left
  out of the reply.
- **Revocation:** a recording policy allows, then denies, on the same client and
  object; the second request and the next delivery see the denial.
- **Reset:** policies installed before `run_core` and before a reset are present
  after it, in order, and hear generation end; #321's host list carry-over test
  still passes.
- **Admission:** two file cookies give two distinct credentials, each stable across
  connections; a session cookie and a host-list fallback give their own record,
  through the setup path. Nothing exposes the secret.
- **Ownership:** the resolver answers for each checked type, retained resources
  included, and `KillClient` on a colormap keeps its `BadValue`.
- **Inline notifications:** a denied selected event is dropped and the reply and
  the remaining events are byte-identical.
- **Lifecycle:** the policy sees connected, connection gone, owner gone and
  generation end in that order, with a retained owner in between.
- **Server work is not checked:** a denying policy leaves hit testing, reset,
  disconnect and rootless window management working.
- **Hot paths:** a benchmark of a tiny drawing request and event fanout, with an
  empty list and with one policy, to replace the current inference with numbers.

Each new test gets the mutation that makes it fail, and the suites run under CPU
load. No hardware run is expected beyond one regression pass on AMD and NVIDIA,
since behaviour must not change before PR 6.

## Known costs

- Until PR 6, no policy is installed in production and the deny branches run only
  in tests.
- Size is an estimate from a source audit, not a measured patch: 1,500 to 3,000
  production lines across the series, plus tests. SECURITY's rules are additional.
- Clipboard isolation remains unfinished, as stated in Non-goals.

## Questions for the maintainer

1. Is one list with "any policy can deny" right for the WM and compositor
   privileges you describe? Under this design a privilege is the absence of a
   denial, granted by each policy for itself, never an override.
2. `DestroySubwindows` keeps today's bulk path with no policy and goes child by
   child with one. XLibre has moved to child by child for everyone, for ordering.
   Should that correction be a separate PR, ahead of this series?

## Provenance

Read for behaviour and scope, with no code copied: XLibre `Xext/xace.c`,
`Xext/xace.h`, `include/dixaccess.h`, `dix/`, `Xext/security/security.c` and
`Xext/namespace/` (X11/MIT licensed; the namespace files are offered under
`X11 OR MIT OR AGPL-3.0-or-later`, used here under MIT). No copyleft source was
consulted.
