# High-Level Design

A modern X11 server written from scratch in Rust. Drives DRM/KMS
directly via Vulkan; runs real desktop environments on modern
Linux, FreeBSD and OpenBSD. On macOS it runs in a window.

## Goals

- Run real X11 desktop environments and window managers.
- Support modern graphics hardware through DRM/KMS and Vulkan,
  with no driver ABI for third-party hardware modules. Primary
  target is Linux; FreeBSD (incl. GhostBSD) and OpenBSD are supported.
- Compositor-native architecture with tear-free presentation by
  default. An eligible fullscreen client requesting asynchronous
  presentation (Present `Async`) may tear.
- Per-output presentation (refresh rate, modes) with multiple
  physical outputs as first-class.
- Implement the modern X11 desktop contract that GTK, Qt, SDL,
  GLFW, Electron, and similar actively used software depend on.
- Single-threaded core (no `Arc<Mutex<ServerState>>` or
  `Arc<Mutex<dyn Backend>>`, no per-client pump threads) so the
  protocol invariants stay obvious.

## Non-Goals

- Being a drop-in clone of Xorg internals.
- Supporting old server driver modules or a DDX-style hardware
  driver ABI.
- Supporting hardware that lacks DRM/KMS + Mesa support (any
  OS).
- Supporting Windows as a runtime target, or DRM/KMS outside Linux
  and the BSDs. On macOS yserver runs in a window on the host's
  display (see Backends), not on the hardware.
- Supporting non-TrueColor legacy visuals as a first-class
  feature.
- Supporting indirect or remote GLX.
- Supporting endian-swapped clients unless a concrete modern use
  case appears.
- Implementing obsolete font functionality beyond what real
  clients still need.
- Preserving behavior that exists only because of Xorg
  implementation accidents, unless real desktop software depends
  on it.

## Crates

- `yserver-protocol` — wire-level X11 encode/decode. Own schema,
  generated parsers/serializers; no XCB XML.
- `yserver-core` — protocol-level core: clients, resources,
  windows, properties, atoms, selections, input dispatch, RANDR,
  XFIXES, DAMAGE, COMPOSITE bookkeeping, XKB, XInput2. Backend-
  agnostic. The single-threaded core loop and mio poller live
  here.
- `yserver-render` — the GPU rendering core shared by the
  platforms: drawable storage, paint engine, scene compositor and
  frame builder (`crates/yserver-render/src/kms/render/`). Vulkan
  only; no DRM/KMS.
- `yserver` — the binary and the platforms: the standalone DRM/KMS
  server (atomic modesetting, scanout, cursor planes, Present and
  direct scanout, VRR, libinput input thread) and the window-surface
  platform used on macOS. It drives `yserver-render`.

## Core loop

Single-threaded. The core thread owns `ServerState` and the backend
(`&mut dyn Backend`) and runs an mio poller. Listed event sources:

- Per-client X11 sockets (read-side; writes go through a separate
  per-client write queue).
- Input thread `eventfd`-style channel.
- Backend-specific FDs (DRM event FD, signalfd).

Signals are blocked at startup and consumed via signalfd in the
poller. SIGTERM/SIGINT route to a `Message::Shutdown` and let the
loop drain cleanly. SIGUSR1 dumps the scanout to PPM; SIGUSR2
dumps drawables.

## Backends

Two platforms, selected at build time by the target OS.

- **DRM/KMS** (Linux, FreeBSD, OpenBSD) — standalone. Opens
    `/dev/dri/card*`, acquires DRM master, drives atomic
    modesetting, owns the console. This is the production target.
- **Window surface** (macOS) — each output is a window-system
  surface presented through its own Vulkan swapchain (Mesa
  KosmicKrisp on Apple Silicon); an AppKit shell creates the
  windows (`crates/yserver/src/surface/`, `crates/yserver/src/macos/`).
  Rootless mode gives every top-level X window its own native
  window (`crates/yserver/src/rootless/`).

The nested `ynest` backend (running under another X server) has
been removed.

The standalone backend uses **Vulkan directly** for rendering
and dmabuf export, and GBM only to allocate scanout buffers. No
EGL, no Mesa GL. Modes are set via DRM atomic; pageflips drive
retirement.

It runs in a single **direct** session mode: the server opens
`/dev/dri/*` and `/dev/input/*` itself, self-manages DRM master,
and arms `VT_PROCESS` on its controlling console so it cooperates
with VT switching (`drmDropMaster` on release, `drmSetMaster` on
acquire). It needs direct device access — root, or a session in
the `video`/`input` groups.

Input: `libinput` posts cooked events into the core loop's
channel from a dedicated input thread. (The window-surface
platform takes its input from the host window system instead.)

## Target platforms

**Linux** is the primary target and where all current development
happens. KMS bring-up, libinput config, hotkeys
(Ctrl-Alt-F#, Ctrl-Alt-Backspace), and VT acquisition all assume
Linux semantics today.

**FreeBSD / GhostBSD** support landed in 11d87ba. This was possible
as FreeBSD has linux API support for DRM, udev and libinput.

**OpenBSD** is supported through the same DRM/KMS platform.

**macOS** (Apple Silicon, macOS 26 or later) runs yserver in a
window on the host display through the window-surface platform; see
the README for setup. **Windows** is out of scope.

## Rendering model

The rendering core is split into five components. `PlatformBackend`
lives in the `yserver` crate (`crates/yserver/src/kms/render/`);
`DrawableStore`, `RenderEngine`, `SceneCompositor` and `FrameBuilder`
live in the `yserver-render` crate (`crates/yserver-render/src/kms/render/`).
`KmsCore` (at `crates/yserver/src/kms/core.rs`) sits alongside them and owns the X11
protocol bookkeeping that is independent of GPU state (XID maps,
window/pixmap metadata, COMPOSITE redirects, SHAPE regions,
picture records, font/glyphset records, cursor records).

### PlatformBackend (`crates/yserver/src/kms/render/platform.rs`)

Hardware and OS surface. Owns the DRM devices and KMS outputs,
the per-output scanout BO pools (`crates/yserver/src/kms/vk/scanout.rs`),
the page-flip retirement state, libinput, and the output layout.
Its `GpuCore` holds the Vulkan device, the ops command pool, the
submit group and the recyclable fence pool behind `FenceTicket`
(VkFence wrappers with CPU-side lifetime semantics, defined in
`crates/yserver-render/src/kms/render/gpu.rs`).

### DrawableStore (`crates/yserver-render/src/kms/render/store.rs`)

Storage and lifetime for every X11 drawable (windows, pixmaps,
the cursor sprite). Each entry is a Vulkan image plus a layout
state machine, with refcount, damage region, presentation
damage, render fence ticket, and scene-participation flag. The
store is the single source of truth for what GPU memory exists
and who holds references.

### RenderEngine (`crates/yserver-render/src/kms/render/engine.rs`)

Paint operations. All X11 drawing requests — fill, put_image,
get_image, copy_area, copy_plane, image_text, poly_*,
render_composite, render_fill_rectangles, render_composite_glyphs,
render_trapezoids/triangles — record one or more Vulkan command
buffers against the destination drawable's storage. The engine
also owns the pipeline cache, the glyph atlas, the descriptor
pool ring, and the deferred frame builder.

### SceneCompositor (`crates/yserver-render/src/kms/render/scene.rs`)

The composed output pass. Walks the window tree once per
present, builds a draw list (root → mapped descendants →
cursor), and emits one composite command buffer per output.
Buffer-age tracking with per-output history rings clips the
repaint to actually-dirty regions; full redraw is the fallback
when history is too short.

### FrameBuilder (`crates/yserver-render/src/kms/render/frame_builder.rs`)

Per-frame coalescer (introduced in Stage 5 Phase B). Every paint
op records into a deferred `RecordedOp` list on the open frame
instead of submitting its own command buffer. The frame closes
on PRESENT completion, get_image sync wait, scene tick, timeout,
or pin-set saturation; close emits one CB and one
`vkQueueSubmit2` per frame. Collapses thousand-op MATE drag
bursts into one submission per vblank.

## Synchronization

- **FenceTicket**: per-submission Rc-shared handle to a VkFence.
  Holds the CPU-side lifetime of staging buffers, descriptor
  arenas, scratch images, and any resource that must outlive its
  CB. Pool-recycled.
- **Per-ScanoutBo export semaphore**: VkSemaphore exported as a
  SYNC_FD, passed to KMS as `IN_FENCE_FD` so the atomic commit
  blocks until the composite CB has finished writing.
- **Image-layout state machine**: every drawable tracks its
  current Vulkan layout; transitions are explicit barriers
  emitted by the engine.
- **Single graphics queue**: same-queue submission ordering is
  the GPU dependency for compose-after-paint and glyph-upload-
  before-draw. No `vkQueueWaitIdle` on the hot path (only inside
  `get_image`, which is synchronous by protocol).

## Protocol scope

Connection setup, native-endian only. Resource IDs, windows,
pixmaps, cursors, graphics contexts, drawables, atoms,
properties, selections, hierarchy, mapping, focus, grabs,
pointer/keyboard events, crossings, event masks.

Extensions implemented: BIG-REQUESTS, COMPOSITE, DAMAGE, DPMS,
DRI3, Generic Event Extension, GLX (modern direct-rendering only
via DRI3/Present), MIT-SCREEN-SAVER, MIT-SHM, PRESENT, RANDR,
RECORD (device events and client start/death only), RENDER,
SHAPE, SYNC, X-Resource, XC-MISC, XFIXES, XINERAMA, XInput2,
XKB (XKEYBOARD), XTEST. Coverage
is what real clients actually drive — extension versions and
capability sets are conservative where the implementation isn't
mature.

## Compositing model

The built-in compositor is primary. Every paintable drawable has
its own storage that can be damaged, composed, and presented
independently. The root window does not imply a global
framebuffer. COMPOSITE redirect (both Automatic and Manual mode)
is supported; the Composite Overlay Window (COW) is a
first-class scene participant so external compositors (xfwm4
composited, marco) draw to it and see their output on
screen.

Per-output presentation scheduling is in place. Tear-free is the
default. Direct scanout is per output: a client covering exactly one
monitor can scan out there, whatever the other monitors' refresh rates,
while the others compose at their own rates,
with no limit on the number of monitors. A whole-screen window keeps
grouped direct scanout when all outputs share the primary device and
their refresh rates are within 0.5%. A Present selecting another
monitor's CRTC is composed. Tearing has one direct owner at a time and may
belong to a fullscreen client on one monitor while other monitors are on. A
second client asking to tear stays direct with synchronized flips until the
current owner releases tearing. A paused owner keeps tearing. yserver advertises
`PresentCapabilityAsync`, not `PresentCapabilityAsyncMayTear`.

VRR is per output: each output whose monitor reports VRR support enables it
while its own client window scans out directly, so several monitors can run
VRR at once while the others keep their fixed refresh. Xorg's modesetting
driver enables VRR only for a window covering the whole screen, and on every
CRTC at once.

VRR is off by default. `--vrr` honors `_VARIABLE_REFRESH` (format 32,
one nonzero item) for client-owned direct scanout. `--vrr all` ignores
that property on NVIDIA DRM and applies NVIDIA/Mesa application
exclusions; on other drivers it behaves like `--vrr`. Under a
compositing desktop, direct scanout and therefore VRR require the
compositor to unredirect fullscreen windows. On Cinnamon, enable
`org.cinnamon.muffin unredirect-fullscreen-windows` (`true`) in System
Settings → General → “disable compositing for full-screen windows”; this
is off by default. With it off, Muffin composites the game and the
monitor stays at maximum refresh. VRR is per monitor: each capable output
uses VRR while its own fullscreen client scans out directly and asks for
it. Several monitors can use VRR at once; others keep their fixed refresh.
Color management / HDR is a valid future direction but not yet
implemented.

## What's not in this document

- Security model. yserver does not yet enforce per-client
  capability restrictions; a client can read other clients'
  windows, observe global input, and inspect the clipboard. 
