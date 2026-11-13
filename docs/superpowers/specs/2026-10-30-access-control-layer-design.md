# Access control layer

Status: approved by the maintainer on #320. PRs 1, 2 and 3 of the series are
implemented (see the series table); PRs 4 to 6 are not.

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
- Rewriting operations in PRs 1 to 5: property filters, virtual roots, renamed
  selections, image censoring, `forcedBG`. Each needs its own interface later.
  This does not leave untrusted clients unprotected: no untrusted credential
  exists before PR 6, and PR 6 has a gate on these protections, below.
- Who may connect: #321 owns that.

## Reference model

Line references to Xorg in this spec are to xorg-server 21.1.24 unless
another source is named.

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

Privileges for the WM, the compositor and session services are recognized by
each policy they matter to, inside that policy. A policy that grants the WM
something only stops objecting itself; it never overrides another policy's
denial.

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
| Creator XID and owner of a retained displayed cursor | recorded for the host cursor until the backend destroys it |
| Whether a window is a root window | screen state |
| Atom name | atom table |
| A client's peer identity | `ClientState.peer` (#321) |
| How a client was admitted | new, below |
| Opcodes of the request being run, if any | dispatcher |
| Input focus, time of last user input | server state, added only with the policy that reads them |

Advertised descriptors such as RENDER `PictFormat`s are not checked. They are
static server tables without an owner or `ResourceKind`, so a policy cannot
distinguish one client's use from another's. A cursor can remain displayed
after its XID is freed; its retained host cursor keeps the creator's XID and
owner until the backend destroys that cursor. KMS can retain a cursor through
references the core does not track, including a grab cursor, the root default,
or an animated cursor's frames. Existing cursor release points sweep records
whose host cursor is neither core-referenced nor reported alive by the backend.

Server objects are still policy-visible targets: system SYNC counters and
RANDR outputs, CRTCs, modes and providers are asked with owner `SERVER_OWNER`,
so a real client's request on one is checked. This follows Xorg's RANDR
resource lookups (`randr/rroutput.c:478`, `randr/rrcrtc.c:1160`,
`randr/rrmode.c:339-358`, `randr/rrprovider.c:153`). GLX context tags, screens, visuals and FBConfigs are not
resources and are not checked.

**Arbitrary-resource operands.** SYNC `SetPriority` / `GetPriority` resources
and `XISetClientPointer` / `XIGetClientPointer` use a resolver that searches every policy-visible
resource kind. It checks the core-store kinds first, in this order: `Window`,
`Pixmap`, `Gc`, `Font`, `Cursor`, `Picture`, `GlyphSet`, `Dri3Syncobj`. It then
checks the remaining kinds in `ResourceKind` order: `Colormap`, `Region`,
`PointerBarrier`, `SyncCounter`, `SyncAlarm`, `SyncFence`, `Damage`,
`PresentEventSelection`, `ShmSegment`, `GlxContext`, `GlxDrawable`,
`RecordContext`, `RandrOutput`, `RandrCrtc`, `RandrMode`, `RandrProvider`. The
first match wins if the same id appears in more than one store. An unresolved
id is not asked and keeps today's behavior. `KillClient` continues to use
`resource_owner`'s narrower lookup, in its existing order; a colormap or an
extension-only id therefore answers `BadValue` before any access check, as
today, although Xorg's `dixLookupClient` accepts it.

**GLX window alias.** Xorg registers a GLXWindow also under the X window's id
(`glx/glxcmds.c:1192-1195`), so `MakeCurrent` / `SwapBuffers` on that X id ask
`WRITE` on the GLX drawable. yserver keys GLX drawables by the GLX id only
(`CreateWindow` does not validate or track the X window), so the same X id
asks `GET_ATTR` on the core window. The `MakeCurrent` release form with
context 0 asks nothing.

The remaining differences from Xorg are in what installed policies are asked
about: `SetCounter`, `ChangeCounter` and `DestroyCounter` on a system counter
answer `BadAccess` before the policy is asked (Xorg asks first, but the client
sees the same error); Present `SelectInput` with mask 0 and a new event id asks
`CREATE` for a selection Xorg would not allocate.

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

The event hooks have these signatures:

```rust
fn send(
    &self,
    ctx: &AccessContext<'_>,
    sender: ClientId,
    window: ResourceId,
    device: Option<u16>,
    events: &EventBatch<'_>,
) -> Decision;

fn receive(
    &self,
    ctx: &AccessContext<'_>,
    recipient: ClientId,
    subject: EventSubject,
    events: &EventBatch<'_>,
) -> Decision;

pub enum EventSubject {
    Window(ResourceId),
    Drawable(ResourceId),
    Device(u16),
    Global,
}
```

`EventBatch<'a>` borrows an ordered slice of `EventDescription`s; it exposes
event metadata, not encoded bytes, so a client can be denied before the
server encodes its event. `EventBatch` wraps `&[EventDescription]` and exposes
it through `events()`. An `EventDescription` stores `type_code: u8` and
`extension: Option<ExtensionEventDescription>`; extension metadata stores
`major_opcode: u8` and `event_type: u16`. The `type_code` is the core event
type with the send-event bit removed; `event_type` is the extension event code
or XI2 `evtype`. For a GenericEvent,
the major opcode is wire byte 1 and the `evtype` is bytes 8–9 interpreted in
the sender's byte order. `describe_wire_event` is the single function that
builds descriptions from wire events: it resolves ordinary extension event
codes by range through the live extension table and reads GenericEvent
metadata from its wire fields. This supplies the event identity policies
need without encoding per recipient. Xorg's SECURITY policy checks the sender
and window once and, if that is denied, denies the whole batch unless every
event in it is an `UnmapNotify`, `ConfigureRequest` or `ClientMessage`
(`Xext/security.c:869-899`), and its dispatch uses the
extension and `evtype` to identify GenericEvents (`dix/events.c:183-191,
419-435, 459-468`).

### A denial is interpreted by its site

`Deny` does not always mean an X error. As in the reference, each site decides:

| Site | On deny |
|---|---|
| Resource, property, device, client, server on a whole request | the policy's `DenyError` |
| A check per returned entry, such as `XResQueryClients` | the entry is left out and the reply succeeds |
| `ListProperties` property entry | the entry is left out; the reply succeeds |
| Core `SendEvent` or XI `SendExtensionEvent` send check | no delivery; the request succeeds |
| Receive | no delivery and no error |
| Extension access | the extension is reported as absent |
| Extension dispatch | `BadRequest` with value 0, whatever the policy's error. Xorg also answers `BadRequest` (`Xext/xace.c:45-46`), but with `client->errorValue` as the value (`dix/dispatch.c:571-573`); that field is not reset between requests and successful handlers set it (`CreateGC`, `dix/dispatch.c:1559`), so Xorg leaks a stale value. Value 0 leaks nothing |
| `QueryKeymap` and similar reads | the zeroed reply XLibre gives |
| A denied effective MIT-SCREEN-SAVER value operand during activation | the operand is replaced with its neutral value (`None` for a background pixmap or cursor; `CopyFromParent` for a border pixmap or colormap), and saver-window creation continues with the remaining values; no error is reported |
| `MapWindow` / `ReparentWindow` remap | `Success`; the window stays unmapped |
| `MapSubwindows` / `UnmapSubwindows` / `CirculateWindow` derived child | the child is skipped; the request succeeds |
| A derived target in `DestroySubwindows` | the loop stops, the remaining children stay unmapped, and the request succeeds (Xorg `dix/dispatch.c:852`) |
| A child of `RedirectSubwindows` / `UnredirectSubwindows` | the child is skipped; its current redirect state stays unchanged, and the request succeeds |
| A Damage object targeted by `DAMAGE Add` | the object is skipped; it does not accumulate the added region, and the request succeeds |
| A matching cursor targeted by `ChangeCursorByName` | the cursor is skipped; its image stays unchanged, and the request succeeds |
| An entry in `XIBarrierReleasePointer` | the policy's error is returned; earlier entries have already taken effect |
| A `ChangeAlarm` counter | the policy's error; fields applied before the counter remain applied, as they do when the counter is missing |
| A Present `Pixmap` / `PixmapSynced` notify window | the policy's error; nothing is presented |

A check authorizes either a whole request or one entry of a list, and the plan
records which for each site. Rewriting is out of scope, but because sites own
their denial, adding a filter or censor beside a check later does not change
this interface.

For a propagating core `SendEvent`, a denied send check at any candidate
window stops the request successfully. If a window has selected recipients
but every selected recipient is denied by Receive, that window counts as a
non-delivery and propagation continues to its parent, subject to the existing
do-not-propagate mask and effective-focus stop. A denied receive check for the
creator selected by a zero event mask is also a non-delivery, but ends the
walk. These rulings follow Xorg's per-candidate send check and successful
early return (`dix/events.c:5622-5630`), its receive-denied input-client path
and delivery count (`dix/events.c:2291-2295, 2377-2433`), and its creator
delivery result (`dix/events.c:2201-2224`).

### Who is checked

Checks run only for work done on behalf of a client request. Hit testing,
ancestor walks, clipping, damage propagation, reset, disconnect, deferred
completion and rootless window management run as the server and are not
checked. The internal clipboard bridge is a real client and goes through the
same checks; a policy decides how to treat it.

Server-generated events have no sender, so the send check does not run for
them. Their recipients do go through the receive check.

MIT-SCREEN-SAVER attributes saved by `SetAttributes` are deferred work of the
client that installed them. When activation computes the effective values for
the saver window, yserver checks their resource operands as derived targets for
that client. A denied operand is replaced with its role's neutral value, and
activation continues without an error. The deferred-client relationship
follows Xorg's saved `ScreenSaverAttrRec.client` and server-side saver-window
creation (`Xext/saver.c:150-164, 465-564, 846-870`); the effective-operand checks and
neutral replacements are this policy's rule.

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
Work run as `SERVER_OWNER` is server work. Subtree descendants that follow an
authorized ancestor's map, unmap, or destroy are consequences, not derived
targets to check.

### Denial with several targets

Whether earlier work survives a later denial is client-visible, so it follows
Xorg, per request. The plan lists every request with several targets and
records its Xorg ordering.

**`DestroySubwindows` keeps the Xorg ordering, policy or not.** Xorg
(`dix/window.c:1104-1124`) unmaps all children first, then checks and destroys
each one in a loop, and a denial stops the loop: the children already destroyed
stay destroyed, the remaining ones stay unmapped but not destroyed.
`ProcDestroySubwindows` ignores that result and returns `Success`
(`dix/dispatch.c:841-853`), so the client gets no error. yserver follows both:

- yserver already has the same shape: it prepares each child, unmaps them in
  bulk, then destroys them in a second loop. The check goes in that loop,
  immediately before each child's destroy, in the order yserver already
  destroys in. With an empty list nothing changes.
- A denial stops the loop. Children not reached keep their window and stay
  unmapped; their prepared destroy is released with no client-visible effect.
- The request succeeds with no error, as in Xorg. XLibre returns the denial as
  an error; yserver does not follow it here.

XLibre also destroys child by child for everyone, so each child's UnmapNotify
comes right before its DestroyNotify, as the protocol describes; Xorg's own
comment admits the bulk unmap departs from it. That is a defensible change on
its own, discussed separately, and nothing in this series depends on it.

A request that checks all its operands before acting, or that has one target,
does leave the state unchanged on denial.

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

Receive denials for redirect notifications (`MapRequest`, `ConfigureRequest`,
`ResizeRequest`, and `CirculateRequest`) count as delivered, so redirection
remains in effect. Xorg returns success from the recipient path when Receive
denies a selected redirect event (`dix/events.c:2560-2585`). Selection events
also go through Receive in yserver for their recipient: the selection owner
receives a `SelectionRequest`, and the requestor receives a `SelectionNotify`.
Xorg identifies those recipients and emits the events in
`dix/selection.c:285-312`; its SECURITY receive hook takes the recipient,
window and event (`Xext/security.c:901-916`), while the selection handler's
direct writes do not call that hook. In yserver's rootless interception, the
server bridge or window manager is the recipient and is checked as
`SERVER_OWNER` before its queue or state is changed; Xorg skips writing a
`SelectionRequest` to a selection owned by `serverClient` and answers the
requestor as if there were no owner (`dix/selection.c:298-303`).

If Receive denies the owner its `SelectionRequest`, the conversion proceeds as
if the selection had no owner: the server writes the requestor's client a
`SelectionNotify` with property `None`, as Xorg does when there is no owner
(`dix/selection.c:305-311`). Without that answer the requestor would wait
forever. That `SelectionNotify`, like the one written when there is no owner,
goes to the client that issued `ConvertSelection` as the direct result of its
own request and is exempt, like `GraphicsExpose`. A `SelectionNotify` the
owner sends with `SendEvent` follows the `SendEvent` rules.

An event with no window, drawable, or device subject uses `EventSubject::Global`.
This includes SYNC alarm notifications and other subjectless notifications
(Xorg 21.1.24 has no DPMS notification). A client denied an `AlarmNotify` does not learn that
the alarm fired and must read its state with `QueryAlarm`; access to the alarm
itself is already a resource check. Core `MappingNotify` is exempt, as in Xorg,
which writes it with no hook: every client can read the keyboard and pointer
mappings with `GetKeyboardMapping` and `GetPointerMapping`, so a denial would
hide nothing and would only leave the client decoding input with a stale
keymap. Xorg writes these notifications directly
to their clients (`Xext/sync.c:425-465`, `dix/inpututils.c:71-103`). RECORD receives a check only for records that carry
X events; Xorg's recorder distinguishes delivered events from client protocol
data (`record/record.c:658-794`).

MIT-SHM `Completion` events and `CounterNotify` events that unblock an `Await`
are direct results of the initiating request and are exempt, like
`GraphicsExpose`. Xorg writes SHM completion directly after `ShmPutImage`
(`Xext/shm.c:601-613`), sends Await notifications only to the unblocked
client (`Xext/sync.c:470-510, 609-699`), and treats `GraphicsExpose` as a
critical direct result (`dix/dispatch.c:1718-1756`). Present
`CompleteNotify` and `IdleNotify` delivered to the client that issued the
`PresentPixmap` are direct results of its request and are exempt, like SHM
`Completion`: a denied `IdleNotify` would leave that client unable to reuse or
free its pixmap. Present notifications delivered to any other selecting client,
and every `ConfigureNotify`, are checked for that recipient with the event's
window as subject. Xorg writes all three Present notifications to the
selected clients with no check at all (`present/present_event.c:133, 171,
201`); this is a yserver decision, not Xorg behaviour. The legacy event writers in `server.rs`
with no production callers remain out of scope.

**Send.** Core `SendEvent` and XI `SendExtensionEvent` are checked at the
request, with the sender, current candidate window, optional device id, and
descriptions of all supplied events. Core propagation checks each candidate
and stops successfully if the sender is denied there; each selected recipient
then gets a Receive check. XI uses the same candidate and recipient rules with
the request's device id. In particular, yserver checks XI sends at every
propagated candidate as required by this policy, while Xorg checks send access
only for the non-propagated XI path (`Xi/exevents.c:2961-2963`). Xorg's core
resolver demonstrates the corresponding per-candidate check
(`dix/events.c:5622-5641`).

**XTEST.** `FakeInput` has no target window, so it is not a send. It is
authorized at admission by the extension dispatch check and a device write
check, as in XLibre, because the input path keeps only the device and loses the
client.

**Property.** The window property handlers use the property hook. XI1 and XI2
device properties use the device hook; RANDR output properties use resource
checks following Xorg's lookup sites. Requests that read and delete, such as
`GetProperty` with `delete`, ask for both modes. RANDR `GetOutputProperty` and
`GetProviderProperty` requests that read and delete likewise ask `READ | WRITE`
on the output or provider. Xorg asks `WRITE` alone for these RANDR requests
(`randr/rrproperty.c:626`, `randr/rrproviderproperty.c:594-595`); yserver follows the
combined read-and-delete rule. Server writes through `change_property_as_server`
are not checked.

**Extension dispatch and access.** Dispatch has one site in `process_request`,
and access is in `QueryExtension` and `ListExtensions`.

**Server.** `may_manage_access` and `may_read_access` in `process_request/misc.rs`
(#321) ask the server hook before their existing rule, as Xorg's
`AuthorizedClient` does (`os/access.c:1302`, 21.1.24): the hook first, then
`client->local`. Request validation that comes earlier (the invalid-mode
`BadValue`) stays first. With no policy `is_local` still answers `BadAccess`
exactly as before; with a policy installed, a remote attempt reaches it. Other
server sites follow XLibre's call sites.

**Client, device.** Follow XLibre's call sites, mapped to yserver's handlers in
the plan.

**Client state.** The policy is told three things, in this order of a client's
life:

1. Connected, before its first request is dispatched, with its `ClientState.peer`
   and its admission record.
2. Connection gone, before its `ClientState` is removed. Its retained resources
   keep their original owner, so the owner id can still appear in later checks.
3. Owner gone, when the retained client is finally closed down: `KillClient` on
   one of its resources, `KillClient(AllTemporary)`, or a reset. This is Xorg's
   `ClientStateGone` for a retained client (`dix/dispatch.c:3478-3560`); freeing
   its resources one by one does not end it, as in Xorg. Every client whose
   close-down mode is `RetainPermanent` or `RetainTemporary` becomes a zombie at
   disconnect, whether or not it holds resources, and hears owner gone only then;
   a client in `DestroyAll` mode hears it right after connection gone.

A reset keeps the same policy objects and tells them the generation ended, so
they drop what they hold per client.

### The existing transport gate

`reject_non_local_extension_request` is not a permission policy. Its rules come
from the transport, it must apply with no policy installed, and it answers with
a specific error per extension and minor. It stays where it is, unchanged. The
dispatch check runs after the gate and after the maximum-length `BadLength`, as
in Xorg (`dix/dispatch.c:546-549`).

## The series

Each PR lands with its own gate and leaves behaviour unchanged for every client
until the last one.

| PR | Content | Reachable in production |
|---|---|---|
| 1 | Interface, policy list, reset carry-over, context skeleton, lifecycle, admission record, server hook at the #321 sites | Sites run, list empty |
| 2 | Extension access and dispatch checks | Same |
| 3 | **Complete:** Resource and property checks, including creation and several-target requests; XRes and RECORD client specifiers deferred to PR 5 | Sites run, list empty |
| 4 | Send and receive checks, with `EventSubject` | Same |
| 5 | Device and client checks, remaining server sites | Same |
| 6 | SECURITY in one step: protocol, generated credentials, rules, protections for untrusted pixels, dispatch and discovery | First denying policy |

Nothing of SECURITY is wired before PR 6: hiding it from discovery alone would not
be enough, since dispatch selects handlers by opcode. Until then its requests
behave as they do today and no generated credential can be issued or admitted, so
no client can hold an untrusted cookie while only part of its isolation is
enforced. The deny branches run only in tests until then. PR 6 can split further
if its size calls for it, as long as dispatch and credential issue land last.

### Gate before untrusted credentials

The hooks alone do not establish SECURITY's boundary. Two Xorg behaviours that
are not checks keep other clients' pixels away from an untrusted client:

- An untrusted client's windows cannot have background None
  (`Xext/security.c:749-752`, `forcedBG`): on Xorg a None background shows
  whatever was on screen, which may belong to another client.
- `GetImage` on a window is censored (`XaceCensorImage`, `Xext/xace.c:207-313`,
  called from `dix/dispatch.c` and `Xext/shm.c`): the parts of the image outside
  the window's `borderClip` are overwritten, so windows that cover it do not
  show. This is not part of SECURITY: Xorg does it for every client, trusted or
  not, and the protocol leaves obscured contents undefined anyway.

Read checks deny `GetImage` on the root window and on other clients' windows,
as SECURITY's access masks do (`Xext/security.c:81-86`). What they cannot cover
is another client's pixels reached through a drawable the untrusted client is
allowed to read.

So the SECURITY policy spec, before the PR that issues untrusted credentials,
must state for each of the two either how yserver provides it or which
operations are denied to untrusted clients instead. For censoring, providing it
can mean doing it for every client, as Xorg does, with no policy hook. The spec
includes a test that an untrusted client cannot read a trusted client's pixels.

It covers every path that returns pixels from a window and that SECURITY's
extension rules leave open to untrusted clients. Xorg's SECURITY shows them only
XC-MISC and BIG-REQUESTS (`Xext/security.c:66-71`), so by default the paths
are core `GetImage`, `CopyArea` and `CopyPlane`; each extension the SECURITY
spec chooses to expose adds its own.

yserver differs from Xorg in ways that spec must measure rather than assume:
each window has its own storage, filled with a default colour when its
background is None (`resources/pixmap_props.rs`,
`core_loop/process_request/windows.rs`), so the screen does not show through
as on Xorg; and `GetImage` reads the backend's copy of the window or of its
redirected ancestor (`core_loop/process_request/drawing.rs`), whose content
from overlapping windows owned by other clients must be established. Inferiors
owned by another client are a yserver question only: Xorg's `borderClip`
includes them, so Xorg does not censor them either.

Selections stay deferred, as in Non-goals.

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
- **Several policies:** on a single-target request, with allow then deny and
  deny then allow, the operation is denied both ways, the first denier's error
  is reported, and a policy after a denier is not asked. Lifecycle notifications
  reach all of them.
- **Several targets:** with an empty list, and with an allow-all policy,
  `DestroySubwindows` emits today's bytes. With a policy that denies one child,
  every child is unmapped, the children destroyed before the denied one stay
  destroyed, the denied child and the rest keep their windows unmapped, and the
  request returns no error. A propagated send stops at the denied window; a
  denied list entry is left out of the reply.
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

## Answered by the maintainer

1. One list where any policy can deny is right; WM and compositor privileges
   are recognized by each policy they matter to, and a grant never overrides
   another policy's denial.
2. `DestroySubwindows` follows Xorg's ordering with or without a policy;
   child-by-child destruction is a separate discussion.
3. Untrusted credentials wait for a policy spec that says how background None
   and `GetImage` censoring are provided, or which operations are denied
   instead, over the pixel paths open to untrusted clients.

## Provenance

Read for behaviour and scope, with no code copied: Xorg xorg-server 21.1.24
(`dix/window.c`, `dix/dispatch.c`, `Xext/security.c`, `Xext/xace.c`, MIT
licensed) and XLibre `Xext/xace.c`,
`Xext/xace.h`, `include/dixaccess.h`, `dix/`, `Xext/security/security.c` and
`Xext/namespace/` (X11/MIT licensed; the namespace files are offered under
`X11 OR MIT OR AGPL-3.0-or-later`, used here under MIT). No copyleft source was
consulted.
