# macOS/Windows port feasibility — it's a nested (ynest-style) port, not a KMS port

**Date:** 2026-06-17 · **Scope:** architecture assessment (no code changed) · **Trigger:** users asking for macOS/Windows/plan9 support

## Verdict

A macOS/Windows port is a **new nested backend that renders into a native window** (XQuartz/XWin model), **not** a port of the bare-metal KMS server. The `Backend` trait seam is in the right place for this, and the expensive-looking part — the Vulkan layer — is the *reusable* part. The Vulkan stack is only **partially welded** to dmabuf/DRM: device selection and the render engine are portable; all DRM/KMS coupling sits in a clean top layer (`scanout.rs` / `dri3.rs`). The real first-pass difficulty is **event-loop reconciliation** (Cocoa's main-thread tyranny vs. the core's single-threaded poll loop), not graphics. **plan9 is a non-starter — no Rust target.**

## Why nested, not KMS

Users on macOS/Windows want *ynest* (a nested X server in a native window), not bare-metal `yserver`. None of the bare-metal stack exists on those OSes:

| Concern | Linux/FreeBSD | macOS | Windows |
|---|---|---|---|
| Display/scanout | DRM/KMS pageflip + dmabuf + `IN_FENCE_FD` | CoreGraphics/Metal/IOSurface | DXGI/DWM |
| Input | libinput/evdev/udev | IOKit/Cocoa | Win32 raw input |
| GPU | Vulkan native | Vulkan via **MoltenVK** (no KMS scanout) | Vulkan native (no KMS scanout) |
| Session/VT | logind/seatd, VT ioctls | — | — |

Linux↔FreeBSD is cheap (~16 `cfg(freebsd)` sites, mechanical: epoll↔kqueue isolated in `kms/v2/completion_poller.rs`) **only because they share the entire graphics stack** (DRM via drm-kmod, libinput, udev, Vulkan). That cheapness does not generalize to macOS/Windows.

## The seam exists, but the current nested backend is the wrong template

- **Seam:** `Backend` trait at `crates/yserver-core/src/backend/trait_def.rs:295`. `yserver-core` (core loop, protocol, fanout) is platform-agnostic; backends plug in. Most trait methods (VT/seat/connector/pageflip) are no-op-defaulted, so a windowed backend implements roughly a third of them. The `HostInputEvent` / `HostEvent` plumbing into the core is the genuinely valuable, reusable part.
- **Wrong template:** the existing nested backend `HostX11Backend` (`crates/yserver-core/src/host_x11/`) is Xnest-style — an X11-protocol *client* of a host X server (`connect_to_host()` over a socket, presents via `PutImage` op72 / `CopyArea` op62). It pulls in **zero Vulkan** and has **no own rasterizer** (it delegates rasterization to the host X server). On macOS that's circular (nothing to forward to) and it's likely bitrotten / slated for removal. So: write a fresh native backend; reuse the trait seam, not this code.

## VkContext is only partially welded — (B), GO at moderate effort

Trace of `crates/yserver/src/kms/vk/`:

- **Device selection is platform-blind.** `pick_physical_device` (`device.rs:522`) scores by GPU type (discrete > integrated > virtual, reject CPU); no DRM render-node match, no `VK_EXT_physical_device_drm`. Picks a GPU identically on RADV / MoltenVK / Windows.
- **Render engine is fully portable.** Compositing uses `vkCmdBeginRendering` into a plain `VkImage` (dynamic rendering, no render-pass objects) behind a `CompositeTarget` trait (`ops/render.rs:38`: `vk_image()`/`vk_image_view()`/`extent()`/`current_layout()`). No scanout/DRM assumptions in shaders or pipeline. It can render into a swapchain image unchanged.
- **DRM/KMS is a clean top layer.** Only `scanout.rs` + `dri3.rs` carry it. The Vulkan memory export (`vkGetMemoryFdKHR`) is portable; only `PRIME_FD_TO_HANDLE` + `add_fb2` (`scanout.rs:340–370`) are DRM-specific.
- **Real bottleneck = no WSI, by deliberate choice.** `device.rs:173` explicitly omits `VK_KHR_swapchain` ("WSI is out of scope; KMS pageflip is our presentation path"). So a native port is **additive, not a rip-out**: add `VK_KHR_surface` + `VK_KHR_swapchain` + the platform surface ext (`VK_EXT_metal_surface` for MoltenVK, `VK_KHR_win32_surface`), and a `swapchain_present.rs` replacing the `vkGetSemaphoreFdKHR(SYNC_FD)` → `IN_FENCE_FD` handoff with ordinary acquire/present semaphores.
- **One init wrinkle:** `external_semaphore_fd` is a non-`Option` field instantiated unconditionally (`device.rs:284`), but `ash::Device::new` only loads function pointers — init won't fail if the extension is absent; only the *use* sites blow up (`scanout.rs:458`, `dri3.rs:39`, `sync.rs:90`). `cfg`-gate those for the native build — they're the KMS sites being replaced anyway. `external_memory_fd` / `image_drm_format_modifier_ext` are already `Option` with LINEAR fallbacks.

## Vulkan driver requirement per OS

The v2 engine is Vulkan-only (no pixman/software fallback — `from_platform_init` bails if `VkContext::new` fails), so the host needs a working Vulkan device. What that means differs by OS:

| Target | Vulkan source | Notes |
|---|---|---|
| **Windows** | GPU vendor driver's Vulkan ICD (NVIDIA/AMD/Intel all ship one; loader = `vulkan-1.dll`) | ~Ubiquitous on machines with vendor drivers. We don't bundle it. Gap only on bare/stock-MS-display-driver boxes. |
| **macOS** (the OS) | **MoltenVK** (Vulkan→Metal translation), **bundled with the app** as `libMoltenVK.dylib` | No native Vulkan on macOS, ever. Metal is always present → once MoltenVK is bundled, every Mac runs it. Carries MoltenVK's translation quirks (Vulkan-shaped Metal, not a conformant driver). Honeykrisp does NOT apply here (see below). |
| Apple Silicon + **Asahi Linux** | **Honeykrisp** (Mesa, native, conformant Vulkan 1.3/1.4) over the Asahi DRM kernel driver | This is the **bare-metal KMS path**, NOT the nested backend. Plain `yserver` runs here today (Asahi HW-cursor `ENXIO`→SW fallback already in tree). |

**Honeykrisp is a Linux driver, not a macOS one.** It's the conformant Mesa Vulkan driver for Apple Silicon GPUs (built on NVK), but it depends on the Apple DRM kernel driver and runs on **Asahi Linux** — macOS has no DRM, only Metal. So an Apple Silicon Mac gets native Vulkan *only if booted into Asahi Linux* (→ regular KMS `yserver`, no nested backend); a server running *inside macOS* still uses MoltenVK. The split is **OS, not hardware**.

**Software-Vulkan fallback is viable in windowed mode (unlike KMS).** The `ensure_hardware_vulkan_for_scanout` preflight (which refuses lavapipe/llvmpipe, gated by `YSERVER_ALLOW_SOFTWARE_VULKAN`) exists because handing a host-memory buffer to a real GPU's atomic scanout hard-hangs the machine. That failure mode doesn't exist in the swapchain path — no KMS scanout. So `cfg`-gate that preflight off for the native backend, and a driverless box (VM, headless server, stock Windows display driver) can still run via lavapipe / SwiftShader — slow, but functional. Forbidden on KMS, fine in a window.

Refs: [GamingOnLinux — Honeykrisp](https://www.gamingonlinux.com/2024/06/honeykrisp-is-a-new-conformant-linux-vulkan-driver-for-apple-m1/) · [Rosenzweig — Vulkan 1.3 on the M1](https://alyssarosenzweig.ca/blog/vk13-on-the-m1-in-1-month.html) · [Phoronix — HoneyKrisp](https://www.phoronix.com/news/Mesa-HoneyKrisp-October)

## First-pass scope (hold the line)

- **Rooted**, single fixed window = the whole X screen (Xnest/XQuartz-rooted). **Not** rootless (one native window per X window) — ~3× the work, later.
- Resize = recreate swapchain (or lock the size initially).
- **Core-X11 clients only.** Drop the entire DRI3/dmabuf/GLX path — accelerated GL clients are out of scope for v1 (no dmabuf analog on macOS/Windows → no zero-copy client buffer sharing; GLX clients unaccelerated/software).
- Keyboard is the fiddly tail: native keycodes → X11 keycode/keysym via xkb. Budget for it.
- No host clipboard integration, no host WM cooperation. Later.

## Recommended approach + platform order

- **`winit` collapses "macOS *and* Windows" into ~one effort.** It provides window + input + event loop cross-platform; `raw-window-handle` + `ash-window` turn its handle into a `VkSurfaceKHR` in a few lines. Per-OS surface/input code nearly vanishes; both platforms mostly fall out of one backend (modulo MoltenVK surface quirks on mac).
- **The hard part is the event loop, not graphics.** The core is single-threaded and owns its own `mio` poll loop; `winit`/Cocoa want to own the **main thread's** event loop (macOS `NSApplication` is main-thread-tyrannical). Reconciling these — likely via `winit`'s `pump_events` / `ControlFlow::Poll` to interleave — is the one unknown that decides "weekend vs. slog." Investigate `core_loop/run.rs` to estimate.
- **Do Windows first to de-risk:** native Vulkan (no MoltenVK translation quirks) and `winit`'s loop integrates without the main-thread constraint biting. Prove the seam on Windows, then take the Cocoa main-thread fight on macOS with the rest already working. (Test HW: yoga for Windows; air/m4 for macOS.)

## Work spine

1. `winit` window + `VkSurfaceKHR` (via `ash-window`).
2. Feature-gated swapchain creation in `VkContext` (add WSI exts).
3. `swapchain_present.rs` replacing the KMS atomic commit.
4. New `NativeBackend: Backend` mapping `winit` events → `HostInputEvent`, handing the swapchain image to the existing compositor as a `CompositeTarget`.
5. `cfg`-gate the fd-export sites (`scanout.rs:458`, `dri3.rs:39`, `sync.rs:90`).

## Effort estimate

Reading `core_loop/run.rs` + `input_thread.rs` changed the estimate **favorably**: the event-loop reconciliation I'd flagged as the big unknown (Cocoa's main-thread requirement vs. the core's poll loop) is **already solved by the existing architecture**.

### The event-loop "hard part" mostly evaporates

`run_core` (`core_loop/run.rs:334`) is a self-contained thread that owns `state` + `backend`, blocks in `mio::poll`, and receives input **from producer threads over a channel** — `Message::HostInput` on `NOTIFY_TOKEN`. In Direct mode that producer is `input_thread::run` (`lib.rs:300`), a spawned thread doing `libinput event → HostInputEvent → sender.send(Message::HostInput(...))` (`input_thread.rs:399`). `CoreSender` is `Send` + clonable (`clone_handle`).

So the native architecture is a re-cast of what already runs in production:
- **`winit` owns the main thread** → satisfies macOS `NSApplication`-on-main-thread for free.
- **`run_core` runs on a spawned thread** (thread-agnostic; needs only the channel + its `Poll`).
- **`winit`'s event handler is the input producer**: native event → `HostInputEvent` → `sender.send(...)`. Identical to the libinput thread. No interleaving of two pollers, no inversion-of-control fight.

Resize is already wired for nested: `HostEvent::Configure → handle_host_container_resize → apply_screen_size_side_effects` (`run.rs:859`) — a `winit` `Resized` maps straight onto it. `Backend: Send` is required by the trait and already satisfied (`Arc` in VkContext/FenceTicket), so moving the swapchain-owning backend to the core thread is fine.

One real ordering wrinkle: the `VkSurfaceKHR` must be created from the window (main thread; on macOS MoltenVK wants `CAMetalLayer` creation there), then handed to the core thread; present-queue selection now needs the surface, so `VkContext::new` gains a surface arg. Minor reorder, not a redesign.

### Phased estimate (rooted single window, core-X11 clients, Windows-first)

| Phase | Work | Effort | Risk |
|---|---|---|---|
| 0 | Skeleton spike: `winit` main thread + spawn `run_core` + push a synthetic `HostInput` through the channel and watch it fan out | ~1 day | **Low** — validates the whole architecture cheaply |
| 1 | WSI in `VkContext`: feature-gate `VK_KHR_surface`/`swapchain` + platform surface ext, add present-support to queue selection, create swapchain | 2–4 days | Med |
| 2 | `swapchain_present.rs`: acquire/present replacing the KMS commit; reconcile the present cadence (`before_block`/`maybe_composite`/`on_page_flip_ready`/`next_wakeup`); swapchain recreate on resize; compositor renders into the swapchain image as a `CompositeTarget` | 3–5 days | **Med-High** — the one genuinely new design (frame cadence + recreate) |
| 3 | `NativeBackend: Backend` (~⅓ of methods; VT/seat/connector/hotplug all no-op) + input mapping. Keyboard is the fiddly tail: `winit` keycodes → X11 keycodes + an xkb keymap | 3–5 days | Med — xkb mapping is the time sink |
| 4 | `cfg`-gate the fd-export sites; Windows build/CI; then the macOS MoltenVK + main-thread-surface tail | 2–3 days (+mac tail) | Med |

**Rough total: ~2–3 weeks of focused solo work to a Windows first cut.** macOS is mostly "the same backend + the MoltenVK/CAMetalLayer surface tail" once Windows is proven — a few extra days, not another project.

### Net

No architectural unknowns remain. Cost concentrates in **Phase 2** (present cadence/swapchain) and **Phase 3** (xkb keyboard mapping) — both bounded. **Do the Phase 0 spike first**: it de-risks the entire premise for a day's effort. Two things to confirm before committing: (1) the present-cadence hooks (`maybe_composite`/`next_wakeup`) map cleanly onto swapchain present rather than fighting it; (2) the MoltenVK present-queue/surface ordering on macOS.

## plan9

Non-starter at the compiler, not the graphics stack: no Rust target (no `*-plan9` triple, no std port). `rustc`/LLVM don't emit for it. Off the table unless someone does a heroic std port first.

## Decoupled cleanup note

If `ynest`-with-X11-host is retired (low value as a solo dev), scope the removal to kill the **X11-host transport** while **preserving the `Backend` seam + `HostInputEvent` plumbing** — otherwise the macOS/Windows on-ramp gets pruned with it.
