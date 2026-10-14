# macOS port, M3: a window you can live in

**Branch:** plan on `feat/macos-m0`, on top of M2 (`7753c62f` + eiger fixes up to `30213b61`; verified on eiger:
key table incl. ISO swap and right-hand modifiers, Cmd-Q to X, twm/xterm/xev, keys propagate from child windows,
single motion per NSEvent). Needs a codex review before code moves. Style and gate follow
`2026-10-14-macos-m0-m1.md`; M3 ends with 1.8.0.

**Goal:** the rooted window behaves like a desktop app. It resizes and the X screen follows (and `xrandr` can resize
the window), it is sharp or comfortably sized on a Retina screen by choice, copy/paste works between macOS and X, the
close button asks before killing connected clients, and there is an app bundle that starts without `just`.

**Non-goals:** rootless (M4), GLX (M4), several X screens or several windows, IME, launchd socket activation of
`DISPLAY` (the XQuartz `org.xquartz:0` model; see open question 5), notarisation/signing for distribution,
Windows.

Tags: **[C]** = checked in code at `30213b61` (file:line). **[A]** = assumption about macOS/AppKit, verified on
eiger before the task that depends on it merges.

## Facts the plan rests on

- **The surface has one fixed-size output** [C]. `SurfacePlatform.fb_size` is set once (`surface/platform.rs:37`).
  RandR state comes from `single_output_randr(fb_size, refresh)` (`kms/render/backend/seam.rs:577`), one output
  `WINDOW-0`, one mode, `mm_width = mm_height = 0` so the core synthesises 96 DPI (`yserver-core/src/randr.rs:431`).
- **Client-driven resize is a silent lie today** [C]. `RRSetScreenSize` validates, calls
  `Platform::set_logical_screen_size` (core `randr_ext/screen.rs:361`), whose default is `Ok(())`
  (`seam.rs:353`); the surface does not override it. The core then updates `state.randr`, resizes root and COW and
  sends ConfigureNotify + RRScreenChangeNotify (`apply_screen_size_side_effects`, `core_loop/run.rs:3773`). Growing
  passes `screen_size_would_crop` (`randr.rs:653`), so `xrandr --fb 2000x1200` "succeeds" while root storage,
  slots and swapchain stay at the old size. `apply_crtc_config` defaults to `Ok(false)` (`seam.rs:329`), treated as
  success; the M0/M1 plan said config requests are refused — they are not.
- **KMS has the storage half** [C]. `apply_virtual_screen_extent` (`kms/render/backend/kms/randr.rs:1303`)
  unflips a direct frame, bumps the topology epoch, sets `platform.fb_w/fb_h`, updates the input extent, reallocates
  and refills root and COW storage (detach → decref → allocate → fill) and invalidates scanout damage. Only the
  first and the `fb_w/fb_h` lines are KMS-specific.
- **Host-driven resize exists for the nested backend only** [C]: `handle_host_container_resize`
  (`run.rs:3532`, `pub(crate)`) = `randr.resize` + changed-output notify + `apply_screen_size_side_effects`. The
  `yserver` crate cannot call it, nor `apply_screen_size_side_effects` (`pub(crate)`).
- **Shell → core channel** [C]: `ShellEvent` has `Visible` and `LockState` (`surface/link.rs:17`);
  `Platform::on_platform_event` drains them with `&mut ServerState` (`surface/platform.rs:584`). Core → shell is
  `ShellUi` (`surface/ui.rs:34`: `set_title`, `quit`, `set_cursor`, `warp_pointer`), executed on the main thread.
- **Swapchain** [C]: `recreate_swapchain` (`surface/swapchain.rs:589`) follows `caps.current_extent` and already
  runs on `SUBOPTIMAL`/`OUT_OF_DATE` (`:322`, `:333`, `:386`). The offscreen slots (`targets`) and their extent are
  fixed at `new()` (`:140`); `record_copy` copies slot → swapchain image 1:1. Nothing sets the `CAMetalLayer`
  `drawableSize` after creation (`macos/window.rs`).
- **Points vs pixels** [C]: the X screen is in backing pixels; the window is `px / backingScaleFactor` points;
  pointer input converts with `x_pt * scale` (`surface/input/pointer.rs:27`); `ShellInput` keeps the size given at
  startup (`macos/app.rs`). The window style has no `Resizable`. No code handles a backing-scale change (window
  dragged between a Retina and a non-Retina screen). There is no `--scale`.
- **Selections are a pure client relay** [C] (`process_request/selection.rs`): the server never owns a selection
  or reads property data, and there is no internal-client precedent (`ClientId(0)` only owns server resources).
- **Fonts** [C]: fontconfig builds the core-font catalog (eiger: 16618 XLFDs from Homebrew fontconfig, measured in
  M0); `default_font_path` (`kms/core.rs:730`) already lists `/opt/X11/share/fonts/*` and
  `/opt/homebrew/share/fonts`, admitted only when a `fonts.dir` exists; `fixed`, `cursor`, `nil2` and 6x13 are
  embedded.
- **Quit** [C]: close button → `applicationShouldTerminateAfterLastWindowClosed` → `applicationShouldTerminate:` →
  `request_shutdown` → `TerminateLater` → `ShellUi::quit`. No confirmation; no client count reaches the shell
  (`state.clients.len()` lives in the core only).
- **Packaging** [C]: only `just macos-run`, which sets `VK_ICD_FILENAMES` (KosmicKrisp ICD under
  `/opt/homebrew/share/vulkan/icd.d/`) and `DYLD_FALLBACK_LIBRARY_PATH=/opt/homebrew/lib`. No `Info.plist`, no
  bundle.

## Decisions

### 1. One size of truth: the window's content in X pixels

The X screen size is always the window's content size in X pixels (`content_points × backing_scale / x_scale`,
decision 3). Both directions converge on one platform function, `resize_screen(w, h)`, which reallocates surface
state and then the core publishes the change:

- **Host → X.** The window becomes `Resizable`. The view reports its new size; the shell posts
  `ShellEvent::Resized { px_w, px_h, backing_scale }`. During a live resize the shell posts at most one event per
  display-link tick and always one at `viewDidEndLiveResize`, so clients see a bounded stream, not one per mouse
  event. `on_platform_event` calls `resize_screen`, then a new `pub` core entry
  `yserver_core::core_loop::host_screen_resized(state, backend, w, h)` — the body of
  `handle_host_container_resize`, which the nested backend then also calls.
- **X → host.** The surface overrides `set_logical_screen_size(w, h)`: `resize_screen(w, h)`, then asks the shell to
  resize the window (`ShellUi::set_content_size(points)`). The window's own resize event that follows is recognised
  (same pixel size) and dropped, so there is no loop. If the window cannot grow that far (larger than the screen's
  visible frame), the window is clamped and the X screen keeps the requested size: the view shows the top-left
  part, as Xephyr does with a too-large `-screen`. **[open question 2]**
- **Modes.** The output lists the current size as its preferred mode plus a fixed list of common sizes
  (1024x768, 1280x800, 1440x900, 1680x1050, 1920x1080, 1920x1200, 2560x1440, 2560x1600, capped at the visible frame
  in X pixels), so `xrandr -s 1280x800` and desktop display settings work. A host resize to a size not in the list
  adds it as the current mode (like Xwayland's resizable mode). `RRSetCrtcConfig` with one of the listed modes =
  `resize_screen` + window resize; anything else (rotation, a second output) gets `BadMatch`, matching the
  M0/M1 plan's "refused".
- **Size range.** `RRGetScreenSizeRange` reports min 320x200 and max = the Vulkan max image extent clamped to
  8192 per side.

### 2. `resize_screen` is the KMS storage code, lifted

`apply_virtual_screen_extent`'s platform-neutral body (unflip, epoch, input extent, root/COW reallocation and
refill, damage invalidation) moves to `kms/render/backend/portable/` as `resize_root_storage(w, h)`; KMS keeps its
`fb_w/fb_h` lines and calls it. The surface version adds: `fb_size`, the output's slot images (reallocated at the
new extent, previous content dropped — the next composite repaints everything), the `CAMetalLayer` `drawableSize`
(via `ShellUi`, main thread), and marks the swapchain for recreation. `record_copy` then copies at the slot extent;
the slot and the swapchain extent can disagree for one frame during a live resize — the copy uses the smaller of the
two per axis and the uncovered part stays the layer's previous content.

### 3. HiDPI: `--scale` is X pixels per point; default 2 on Retina **[open question 1]**

Today one X pixel = one backing pixel: sharp, but every X app is half size on Retina, and almost no X client reads
DPI. `--scale N` (N ∈ {1, 2}, default **auto** = the window's `backingScaleFactor`, i.e. 1 X pixel = 1 point) sets
how many backing pixels one X pixel covers per axis:

- `--scale 1`: the M1 behaviour (X px = backing px).
- `--scale 2` (auto on Retina): X px = points; the swapchain stays at backing resolution and the slot → swapchain
  copy becomes a `vkCmdBlitImage` with `NEAREST` filtering at 2×. Integer only, so text stays crisp-blocky rather
  than blurred. **[A]** KosmicKrisp supports `BLIT_DST` on the swapchain format (vulkaninfo on eiger: B8G8R8A8
  swapchain has `TRANSFER_DST`; blit support is the format feature `BLIT_DST`, checked in task 3).
- Pointer, scroll, cursor image and `warp_pointer` divide/multiply by the same factor; the cursor image is scaled by
  AppKit (`NSImage` size in points).
- A backing-scale change (`viewDidChangeBackingProperties`) with `--scale auto` keeps the X screen size and changes
  only the copy factor; with an explicit `--scale` the X screen is resized so the window keeps its point size.
- `--geometry WxH` stays in X pixels (M1 decision); the default becomes 1280x800 X pixels.

DPI: the output reports real millimetres derived from `CGDisplayScreenSize` of the window's display and the
window's share of it, so `xdpyinfo` shows ~110 DPI at `--scale 2` on a 220 DPI panel and ~220 at `--scale 1`. Apps
that honour DPI (Xft.dpi via xrdb is the user's business, not ours) see the truth.

### 4. Clipboard: an in-process X client, the xpbproxy model

XQuartz bridges with `xpbproxy`, an ordinary X client. We do the same: a `clipboard` thread connects to our own
`DISPLAY` over the Unix socket as a normal client. That keeps the selection code in the core untouched and makes
the bridge an X client we can test on Linux against the headless server.

- **X side (OS-neutral, `surface/clipboard/`):** a minimal hand-written X client on `yserver-protocol` encoders
  (no new dependency; it needs InternAtom, CreateWindow, SetSelectionOwner, ConvertSelection, GetProperty,
  ChangeProperty, SendEvent, XFixes SelectSelectionInput). It watches CLIPBOARD (and PRIMARY, open question 3)
  with XFixes; on an owner change it converts to `UTF8_STRING` (fallback `STRING`), handles `INCR`, and hands the
  text to the host. When the host clipboard changes it takes ownership of CLIPBOARD and answers `TARGETS`,
  `UTF8_STRING`, `STRING`, `TEXT`, `TIMESTAMP`, using `INCR` above the max request size.
- **Host side:** a `Pasteboard` trait (`read_text`, `write_text`, `change_count`); macOS implements it with
  `NSPasteboard generalPasteboard` on the main thread (posted through `ShellUi`); the X side polls `change_count`
  every 250 ms (NSPasteboard has no change notification). A loop guard drops the echo of our own writes in both
  directions.
- Text only in M3. Images/files are M4+.

### 5. Close button and Quit ask while clients are connected (maintainer, binding)

The core publishes the connected-client count to the shell (an `Arc<AtomicUsize>` in `SurfaceBackendState`,
updated where `state.clients` gains or loses a client; the internal clipboard client is not counted).
`windowShouldClose:` and `applicationShouldTerminate:` show an `NSAlert` ("N X clients are connected. Quit
yserver?" Quit / Cancel) when the count is > 0; Cancel keeps everything running. SIGTERM never asks.
`applicationShouldTerminateAfterLastWindowClosed` stays `true`, so close = quit after the alert.

### 6. Fonts: verify, fix only what fails

Run `xlsfonts | wc -l`, `xterm -fn fixed`, `xterm -fa Monospace`, `xfontsel` and `xlogo` on eiger with and without
XQuartz installed. If the Homebrew font dirs are useless without `fonts.dir`, admit directories that contain PCF/BDF
files by generating the font table in memory (we already parse PCF for the embedded fonts) instead of requiring
`fonts.dir`. No other font work.

### 7. Packaging: an app bundle that finds Homebrew by itself

`just macos-bundle` builds `target/yserver.app` (`Contents/MacOS/yserver`, `Info.plist` with
`LSMinimumSystemVersion`, `NSHighResolutionCapable`, an icon). The binary no longer needs the `just` environment:
when `VK_ICD_FILENAMES`/`VK_DRIVER_FILES` are unset it points the loader at the KosmicKrisp ICD under the Homebrew
prefix (`brew --prefix` at build time, `/opt/homebrew` fallback), and it already finds the Homebrew loader
(`load_entry` fallback). Homebrew dylibs are not bundled (requires `brew install mesa libxkbcommon fontconfig`);
documented in the README's macOS section. Launched from Finder, the app picks a free display, shows `DISPLAY=:N`
in the window title, and logs to `~/Library/Logs/yserver.log`.

## Gate (every task; **G**)

As M2: nightly fmt; `cargo clippy --all-targets -- -D warnings` (+ `--features xdmcp`) on Linux and macOS;
`cargo test --workspace --all-targets --locked` on Linux and eiger; lavapipe `--ignored` on Linux;
`tools/check-layout.sh`. Each task lists its eiger check; nothing is squashed into M3 before eiger passes it.

## Tasks

1. **Resize core plumbing (Linux-testable).** `pub host_screen_resized` in core (nested backend switched to it);
   `resize_root_storage` lifted out of `apply_virtual_screen_extent` (KMS behaviour unchanged — existing KMS
   resize/hotplug tests are the oracle); surface `resize_screen` + `set_logical_screen_size` override; mode list and
   size range; `RRSetCrtcConfig` BadMatch for unsupported configs. Tests (headless surface on lavapipe):
   RRSetScreenSize grow and shrink reallocate root (GetImage of the new area returns the root background, not an
   error/zero), root ConfigureNotify and RRScreenChangeNotify carry the new size, `xrandr -s` mode switch, BadMatch
   cases (expected errors taken from Xorg `randr/rrcrtc.c`, not guessed). **G.**
2. **Host resize on macOS.** `Resizable` style, view size → `ShellEvent::Resized` (tick-coalesced),
   `drawableSize` update, `ShellInput` size update, `ShellUi::set_content_size`, loop guard. Eiger: drag the corner,
   twm's root menu and an `xterm -geometry 80x24-0-0` follow; `xrandr -s 1024x768` shrinks the window;
   `xrandr --fb 3000x2000` larger than the screen (behaviour per open question 2).
3. **`--scale`.** Option parsing (`auto|1|2`), blit path with `NEAREST`, input/cursor/warp factor, backing-scale
   change, real mm. Linux unit tests for the coordinate math (both factors, both edges). Eiger: `--scale 1` and
   `--scale 2` side by side; xeyes and twm drag land under the macOS pointer at both; drag the window to a
   non-Retina external screen (if available) with `auto`.
4. **Close confirmation.** Client count plumbing + `NSAlert` in `windowShouldClose:` / `applicationShouldTerminate:`.
   Linux test: the count follows connect/disconnect. Eiger: close with 0 clients quits silently; with xterm open it
   asks, Cancel keeps xterm alive, Quit ends both; SIGTERM never asks.
5. **Clipboard.** X-side client + `Pasteboard` trait with a fake; Linux tests against the headless server, with
   a second hand-written client as the other X party: X→host, host→X, INCR both ways, owner change, echo guard.
   macOS `NSPasteboard` impl. Eiger: `xterm -xrm 'XTerm*selectToClipboard: true'`, select text, Cmd-V in TextEdit;
   Cmd-C in TextEdit, paste in xterm (middle button with `selectToClipboard`); `xclip -o -selection clipboard`
   both ways.
6. **Fonts.** Eiger survey (decision 6); fix only what fails.
7. **Bundle.** ICD self-discovery, `just macos-bundle`, `Info.plist`, Finder launch, log file, README section.
   Eiger: double-click the app with no terminal, `DISPLAY=:N xterm` from Terminal connects.
8. **Release prep.** Squash M3, status entry, 1.8.0 notes input for jos (no tag, no publish — jos's call).

## Risks

- **Storage lift regresses KMS resize/hotplug** (task 1). Mitigation: no behaviour change in KMS; the existing KMS
  RandR tests and a dual-output HW smoke on silence before squashing.
- **Live-resize churn.** Each resize reallocates root and slots; GTK/Qt clients repaint on every ConfigureNotify.
  Coalescing to display-link ticks bounds it; if it is still heavy, apply on `viewDidEndLiveResize` only and stretch
  the old frame meanwhile.
- **The in-process clipboard client** counts as a client for X (shows in `xlsclients`), holds a connection slot and
  must not keep the server alive or block shutdown; it is started after the listener and stopped first.
- **Blit support** on KosmicKrisp for the swapchain format **[A]**; fallback = a fullscreen draw (the existing
  `ScalePassPipeline`).

## Open questions (maintainer / codex)

1. **Default `--scale` on Retina:** `auto` (= 2, X px = points, readable, softer) or 1 (sharp, tiny)? XQuartz
   defaults to points (it has a "full resolution" toggle).
2. **X screen larger than the host can show** (`xrandr --fb 4000x3000`): clamp the window and show the top-left part
   (Xephyr), refuse with BadValue, or scale down to fit?
3. **PRIMARY:** sync PRIMARY → macOS (XQuartz option "Update Pasteboard when CLIPBOARD/PRIMARY changes"), or
   CLIPBOARD only? Proposal: CLIPBOARD both ways; PRIMARY → pasteboard behind a flag, off by default.
4. **Bundle scope:** Homebrew dependency (this plan) vs. bundled dylibs + ICD (`dylibbundler`) for a drag-and-drop
   app. Bundling is bigger and touches signing; proposal: M4.
5. **launchd `DISPLAY` activation** (start yserver on first X client, like XQuartz's `org.xquartz:0`): conflicts with
   XQuartz if both are installed. Proposal: M4, opt-in.
