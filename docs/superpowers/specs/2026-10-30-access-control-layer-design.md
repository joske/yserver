# Access control layer

Status: draft for discussion with the maintainer. Nothing is implemented.

## Why

Discussion #123 asks how to manage permissions between X clients. The
maintainer's answer sets the order: first an access control layer and the
SECURITY extension; client isolation is a longer-term goal. This spec covers
the layer only. SECURITY comes in a later PR, as the layer's first permission
policy.

Today a connected client can read another client's window, listen to the
keyboard, inject input and read properties and selections. The only checks
are setup authorization and a transport gate on three extensions. Nothing in
yserver can answer "may this client do this operation on this object?", so
any future rule would have to add its own scattered checks, and one missed
site is a hole.

## Goals

- One interface that answers that question, consulted on behalf of a client at
  every operation SECURITY and Xnamespace both control.
- No policy that denies is shipped. With no policy installed, every byte a
  client observes stays the same, including error precedence.
- Tests prove each check site really denies, using an installed test policy
  through the production dispatcher.
- The interface does not block what SECURITY and Xnamespace need later.

## Non-goals

- The SECURITY extension, trust levels, authorization identity retained per
  client, and any user-visible option.
- Client isolation and Xnamespace.
- `xhost` and `SetAccessControl` (#305): those decide who may connect, not what
  a connected client may do.
- Rewriting operations: property and selection filters, virtual roots, renamed
  selections, image censoring, `forcedBG`. Each needs its own interface later.
- Portals, PipeWire.

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
filter, which is a non-goal here.

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

`ServerState` holds `Option<Arc<dyn AccessPolicy>>`, `None` by default, with an
ordinary installer method. Tests install through it; production installs
nothing in this PR. There is no environment variable and no `cfg(test)` in
production bodies. `Reset` builds a new `ServerState`, so it must carry the
policy over.

The check helpers return `Allow` without a dynamic call when no policy is
installed, the same null-list shortcut XLibre has, so the hot paths pay a branch.

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
| Server | client, mode |
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
does for every child in XLibre. Traversing to find them stays unchecked server
work.

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

**Server, client, device.** Follow XLibre's call sites, mapped to yserver's
handlers in the plan.

**Client state.** The policy is told three things, in this order of a client's
life:

1. Connected, before its first request is dispatched.
2. Connection gone, before its `ClientState` is removed. Its retained resources
   keep their original owner, so the owner id can still appear in later checks.
3. Owner gone, when its last retained resources have been destroyed.

A reset keeps the same policy object and tells it the generation ended, so it
drops what it holds per client. The policy object itself survives the new
`ServerState`.

### The existing transport gate

`reject_non_local_extension_request` is not a permission policy. Its rules come
from the transport, it must apply with no policy installed, and it answers with
a specific error per extension and minor. It stays where it is, unchanged, and
the dispatch check runs right after it.

## Testing

- **Bytes unchanged:** the existing suite passes unmodified, including the
  gate's tests, with no policy installed.
- **Every site denies:** a table-driven test through `process_request`, one row
  per request or event family, installs a denying policy and asserts the
  site's result from the table above, plus no side effect: nothing reached the
  backend and the state is unchanged.
- **Every site asks:** a recording policy asserts each request presents the
  expected client, object and mode.
- **Derived targets and lists:** a denying policy stops `DestroySubwindows` on a
  denied child, stops a propagated send at the denied window, and leaves the
  denied entry out of a list reply.
- **Inline notifications:** a denied selected event is dropped and the reply
  and the remaining events are byte-identical.
- **Lifecycle:** the policy sees connected, connection gone, owner gone and
  generation end in that order, with a retained owner in between.
- **Server work is not checked:** a denying policy leaves hit testing, reset,
  disconnect and rootless window management working.
- **Hot paths:** a benchmark of a tiny drawing request and event fanout, with
  and without a policy, to replace the current inference with numbers.

Each new test gets the mutation that makes it fail, and the suites run under
CPU load. No hardware run is expected beyond one regression pass on AMD and
NVIDIA, since behaviour must not change.

## Known costs

- Until SECURITY lands, nothing in production uses this layer, and the deny
  branches run only in tests. This was accepted in the design discussion.
- Size is an estimate from a source audit, not a measured patch: 1,500 to 3,000
  production lines, plus tests.

## Questions for the maintainer

1. Does "access control layer" mean this, the per-operation checks? The other
   reading is the connection check of #305.
2. One PR, or a series (interface and dispatch, then events, then resources and
   the rest)? The estimated size spreads across many files.
3. Should selections get a plain check now, even though neither SECURITY nor
   Xnamespace would use it?

## Provenance

Read for behaviour and scope, with no code copied: XLibre `Xext/xace.c`,
`Xext/xace.h`, `include/dixaccess.h`, `dix/`, `Xext/security/security.c` and
`Xext/namespace/` (X11/MIT licensed; the namespace files are offered under
`X11 OR MIT OR AGPL-3.0-or-later`, used here under MIT). No copyleft source was
consulted.
