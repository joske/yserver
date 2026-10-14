# macOS port, M3: a window you can live in

**Branch:** plan on `feat/macos-m0`, on top of M2 (`7753c62f` + eiger fixes up to `30213b61`; verified on eiger:
key table incl. ISO swap and right-hand modifiers, Cmd-Q to X, twm/xterm/xev, keys propagate from child windows,
single motion per NSEvent). Needs a codex review before code moves. Style and gate follow
`2026-10-14-macos-m0-m1.md`; M3 ends with 1.8.0.

**Goal:** the rooted window behaves like a desktop app. It resizes and the X screen follows (and `xrandr` can resize
the window), it is sharp or comfortably sized on a Retina screen by choice, copy/paste works between macOS and X, the
close button asks before killing connected clients, and there is an app bundle that starts without `just`.

**Non-goals:** rootless (M4), GLX (M4), several X screens or several windows, IME, launchd socket activation of
`DISPLAY` (the XQuartz `org.xquartz:0` model; M4, opt-in), notarisation/signing for distribution,
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

### 1. Two sizes: the root (X screen) and the output (the window)

The surface keeps two sizes apart, as KMS does with a virtual screen larger than its CRTCs:

- **Root size** = the RandR screen size = root window and COW storage. Changed by `RRSetScreenSize` and by host
  resizes. Always contains the output.
- **Output size** = the `WINDOW-0` CRTC's mode = the window's content in X pixels
  (`content_points × backing_scale / x_scale`, decision 3). The slots and the swapchain follow the output, not the
  root. The CRTC sits at (0,0), so a root larger than the output is shown cropped to its top-left part (Xephyr's
  answer to a too-large `-screen`).

The three ways a size changes:

- **The user resizes the window (host → X).** The window becomes `Resizable`. The view reports its size; the shell
  posts `ShellEvent::Resized { px_w, px_h, backing_scale, request: None }`, at most once per display-link tick during
  a live resize and always at `viewDidEndLiveResize`. `on_platform_event` resizes output and root to the window
  (slots, root/COW), then calls the `pub` core entry `host_screen_resized(state, backend, w, h)` (`randr.resize`:
  screen + output + CRTC, notifies, root ConfigureNotify). A root enlarged with `--fb` is therefore reset to the
  window on the next user resize. An event whose size equals the current output is dropped. `host_screen_resized`
  today returns early when the *screen* already has the size (`run.rs:3548`); it must compare against both the
  screen and the output mode, and notify only what changed (root ConfigureNotify/ScreenChange when the screen
  changed, CrtcChange/OutputChange when the output did). Test: `--fb 1600x900` with a 1024x768 output, then a user
  resize to 1600x900 → the output becomes 1600x900, the screen is untouched and gets no ConfigureNotify.
- **A client sets the screen size (`RRSetScreenSize`).** Root only; the output and the window do not change. A size
  smaller than the output is BadMatch (the core's existing crop check). No window event follows, so there is no
  loop.
- **A client switches mode (`RRSetCrtcConfig`, `xrandr -s`).** The mode must fit in the root (BadMatch otherwise —
  `xrandr -s` sets the screen size first). The output takes the mode (slots resized), then the surface asks the
  shell to resize the window with a request token (`ShellUi::set_content_size(points, token)`). The window's resize
  event carries the token back (`request: Some(token)`); a tokened event only completes the request: if AppKit
  clamped the window, the output (CRTC mode) follows the actual size and a CrtcChangeNotify goes out, but the root
  never changes. So a clamp cannot shrink X behind the client's back, and nothing loops.
- **Modes.** The output lists the current size as its preferred mode plus 1024x768, 1280x800, 1440x900, 1680x1050,
  1920x1080, 1920x1200, 2560x1440, 2560x1600, each only if it fits the screen's visible frame in X pixels, so a
  listed mode is not normally clamped. A user resize to an unlisted size adds it as the current mode (Xwayland's
  resizable mode). Rotation, reflection, a second output or a CRTC position other than (0,0) is BadMatch.
- **Size range.** `RRGetScreenSizeRange` reports min 320x200, max = the Vulkan max image extent clamped to 8192 per
  side.

### 2. Resizes are atomic: allocate everything, then commit

A resize runs in two phases. **Prepare** does every fallible step on objects nothing published refers to yet:
allocate the new root, COW and (when the output changes) slot images; fill and refill the new root and COW (root
background or tile, old content copied in) by GPU work targeting the unattached images; build the new scene output
state for the new layouts (`rebuild_outputs`, `scene/lifecycle.rs:235`, split into a fallible
`prepare_outputs(sink, layouts) -> PreparedOutputs` and an infallible `install_outputs`). **Commit** only swaps
handles and updates `fb_size`, the output layout and `state.randr`; it allocates nothing and submits nothing, so it
cannot fail. If any prepare step fails, everything it made is freed and nothing changes:

- client-driven (`RRSetScreenSize`, `RRSetCrtcConfig`): the request fails with BadMatch and the server keeps its old
  size, as Xorg (`randr/rrscreen.c:289–293`: a failed `RRScreenSizeSet` is BadMatch);
- host-driven (window resize): the window cannot be refused, so the output and the root keep their old size and the
  window shows them cropped or with the uncovered part cleared (`record_copy` copies the smaller extent per axis
  already); one warning is logged, and the next resize event retries.

The shared `resize_root_storage` (portable) is split into `prepare` (allocate, may fail, returns the new storage) and
`commit` (swap + refill + damage, infallible); KMS's `apply_virtual_screen_extent` uses the same split, which also
fixes its detach-before-allocate order. Tests inject a failure at each prepare stage — allocation, the
fill/refill submission (`engine/fill_copy.rs:31` and the copy), and `prepare_outputs` — for both directions, and
assert the old state and, for client requests, BadMatch.

### 3. HiDPI: `--scale` is X pixels per point; default `auto` (= 2 on Retina)

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

XQuartz bridges with `xpbproxy`, an ordinary X client. We do the same: a `clipboard` thread speaks the X protocol
as a normal client. That keeps the selection code in the core untouched and makes the bridge an X client we can test
on Linux against the headless server.

- **Connection, and why it is not counted:** the thread does not connect through the listener. The server creates a
  socketpair, hands one end to the core as an internal connection (the same setup path as an accepted socket, plus
  an `internal` flag on `ClientState`), and gives the other end to the thread. The client-count hook (decision 5)
  counts only clients without the flag, so the bridge never triggers the quit alert; it is otherwise an ordinary
  client (it shows in `xlsclients`, gets SelectionClear, is killed by KillClient and reconnected by the bridge).

- **X side (OS-neutral, `surface/clipboard/`):** a minimal hand-written X client on `yserver-protocol` encoders
  (no new dependency; it needs InternAtom, CreateWindow, SetSelectionOwner, ConvertSelection, GetProperty,
  ChangeProperty, SendEvent, XFixes SelectSelectionInput). It watches CLIPBOARD only (PRIMARY is M4+) with
  XFixes; on an owner change it converts to `UTF8_STRING` (fallback `STRING`), handles `INCR`, and hands the
  text to the host. When the host clipboard changes it takes ownership of CLIPBOARD and answers `TARGETS`,
  `UTF8_STRING`, `STRING`, `TEXT`, `TIMESTAMP`, using `INCR` above the max request size.
- **Host side:** a `Pasteboard` trait (`read_text`, `write_text`, `change_count`); macOS implements it with
  `NSPasteboard generalPasteboard` on the main thread (posted through `ShellUi`); the X side polls `change_count`
  every 250 ms (NSPasteboard has no change notification). A loop guard drops the echo of our own writes in both
  directions.
- Text only in M3. Images/files are M4+.

### 5. Close button and Quit ask while clients are connected (maintainer, binding)

The core publishes the connected-client count to the shell (an `Arc<AtomicUsize>` in `SurfaceBackendState`,
updated where `state.clients` gains or loses a client; clients with the `internal` flag — the clipboard bridge,
decision 4 — are not counted).
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
   `resize_root_storage` lifted out of `apply_virtual_screen_extent` and split into prepare/commit (KMS behaviour
   unchanged except the allocate-before-detach order — existing KMS resize/hotplug tests are the oracle); surface
   root size vs output size (decision 1): `set_logical_screen_size` resizes the root only, slots follow the output;
   mode list, size range, `RRSetCrtcConfig` mode switch (output + slots + tokened window request) and BadMatch for
   unsupported configs. Tests (headless surface on lavapipe): RRSetScreenSize grow and shrink reallocate root
   (GetImage of the new area returns the root background), root ConfigureNotify and RRScreenChangeNotify carry the
   new size, the CRTC mode and the slots keep the output size after `--fb`, `xrandr -s` mode switch, allocation
   failure for both directions (old state kept; BadMatch for client requests), BadMatch cases (expected errors
   taken from Xorg `randr/rrcrtc.c`, not guessed). **G.**
2. **Host resize on macOS.** `Resizable` style, view size → `ShellEvent::Resized` (tick-coalesced),
   `drawableSize` update, `ShellInput` size update, `ShellUi::set_content_size`, loop guard. Eiger: drag the corner,
   twm's root menu and an `xterm -geometry 80x24-0-0` follow; `xrandr -s 1024x768` shrinks the window;
   `xrandr --fb 3000x2000` larger than the screen: the window keeps its size and shows the top-left part of the root.
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

## Open questions — resolved (codex round 1; maintainer may override)

1. **Default `--scale`:** `auto` (X px = points on Retina, readable, visibly pixelated at 2× nearest); `--scale 1`
   for full resolution. The eiger side-by-side of both is an acceptance check of task 3.
2. **X screen larger than the window:** a larger root shown cropped to the top-left (decision 1).
3. **PRIMARY:** CLIPBOARD only in M3.
4. **Bundle:** Homebrew-dependent, documented (decision 7); bundled dylibs later.
5. **launchd `DISPLAY` activation:** M4, opt-in.

## Review round 1 (codex) — resolved

- RandR vs rendered output disagreed after `--fb` (slots followed the root) → decision 1: root and output sizes
  are separate; slots follow the output.
- A failed resize could split the server between sizes; root storage was detached before allocation → decision 2:
  allocate-then-commit, BadMatch on client failure, keep-old on host failure, allocation-failure tests.
- Same-size guard looped on a clamped window → tokened window requests; a tokened event never changes the root.
- The clipboard bridge would be counted by the quit alert → internal socketpair connection with an `internal` flag.

## Review round 2 (codex) — resolved

- `host_screen_resized` returned early when only the output differed (root already at the size after `--fb`) →
  compare screen and output, notify per change, test that sequence (decision 1).
- Prepare covered allocation only; the scene rebuild and the background fill can fail after publication → both
  move into prepare (fill on unattached images, `prepare_outputs`/`install_outputs`), with failure injection per
  stage (decision 2).

## Implementation notes

- **Task 1, review-independent part** (`50f6273f`, `f1a9b7a9`, `4e4009a8`): `pub host_screen_resized` in core
  (nested backend calls it); `resize_root_storage` in `portable/paint_target.rs` (KMS keeps topology epoch, input
  extent and `fb_w/fb_h` — more KMS-only lines than decision 2 listed); surface `resize_screen` +
  `set_logical_screen_size`. `RRSetScreenSize` stays screen-only, as Xorg: the `WINDOW-0` CRTC keeps its mode, so a
  later shrink below the mode is BadMatch until the mode list lands. `record_copy` already copied the smaller extent
  per axis (decision 2 described it as new). Mode list, BadMatch, size range and all AppKit parts wait for review.
- **After review round 1:** `4e4009a8` resizes the slots and the output layout on `RRSetScreenSize`; decision 1
  makes that root-only, and decision 2 makes the storage swap atomic. Both are reworked in task 1 before anything
  builds on them.
- **Task 1, after rounds 1–2** (`675e0376` prepare/commit, then the surface commit): where Xorg disagrees with the
  decisions above, Xorg wins. The surface CRTC is a plain `RRCrtcCreate` CRTC (`rotations = RR_Rotate_0`,
  `transforms = FALSE`, `RandrLimits` in core), so: rotation/reflection → BadMatch (`rrcrtc.c:1403`); a mode past
  the root → **BadValue** with x/y as the value (`rrcrtc.c:1446-1457`), not BadMatch; SetCrtcTransform → BadValue
  (`rrcrtc.c:1088`); position ≠ (0,0) inside the root, disabling the CRTC and a failed prepare of a mode switch →
  status **RRSetConfigFailed** (`rrcrtc.c:1462-1465`, a DDX `crtcSet` failure), not BadMatch. A failed
  `RRSetScreenSize` prepare is BadMatch. A CRTC-only host change still sends ScreenChangeNotify (Xorg `TellChanged`
  sends it on any change); root ConfigureNotify only when the screen size changed. Root/COW are refilled with the
  background/tile on the unattached images; there was no old-content copy before and none is added. Fill failures
  are those `fill_rect`/`render_composite` report when recording; a failed later submit is renderer-fatal
  (`renderer_failed`), not a resize failure. `ShellEvent::Resized` has no `backing_scale` yet (task 3).
- **Task 2** (macOS shell, not compiled here; eiger pending): the view's `setFrameSize:` (plus `windowDidResize:` as
  a backstop, repeats dropped) reports `bounds × backingScaleFactor`, floored. `surface/resize.rs`
  `ResizeCoalescer` decides what to post: every change outside a live resize; during one
  (`viewWillStartLiveResize`/`viewDidEndLiveResize`) the first change, then at most one per frame-clock tick
  (`FrameClock::set_tick_hook` → main queue, only while a size waits), and the final size at the end. A core request
  is answered after `setContentSize:` returns (synchronous): the view holds back its own report meanwhile and the
  window's size goes out under the token even when unchanged, so a no-op request still completes. Each post sets
  `drawableSize` and `ShellInput`'s pointer clamp first; the layer frame follows the view on every change (actions
  disabled) with `contentsGravity` top-left. The core rebuilds every swapchain on every `Resized`, even when the
  output keeps its size, so the swapchain always ends at the last drawable size. Content min size 320x200 X pixels.
- **Task 3** (macOS shell not compiled here; eiger pending): `--scale auto|1|2` (`LaunchOptions.scale`, warned
  as ignored on KMS). `surface/resize.rs` holds the arithmetic: copy factor `N` = `round(backing)` for auto, else
  the given factor; X pixels per point = `backing / N`; the output's millimetres = X size / xpp points × the
  screen's mm per point. `ShellEvent::Resized` carries `scale` (N) and `mm`; every surface takes N
  (`set_copy_scale`) and rebuilds its swapchain. Slots stay X-sized; for N > 1 `record_slot_copy` blits with
  `NEAREST` (src = what fits whole, dst = src × N, the rest cleared), checked by a lavapipe test that reads the
  blit back pixel by pixel. Without `BLIT_SRC` on the slot format or `BLIT_DST` on the swapchain format the copy
  stays 1:1 with one warning — a known gap; the `ScalePassPipeline` fallback is not built. Millimetres go on the
  `WINDOW-0` output only (0 = unknown → 96 DPI); the screen keeps 96 DPI, as Xorg without a DisplaySize
  (`xf86RandR12.c:788-815`), and an mm-only change sends the RandR notifies. The shell (`macos/scale.rs`)
  converts with xpp everywhere (window and min size, `max_content_size`, `setContentSize:`, pointer, warp, cursor
  image and hot spot), sizes the drawable at size × N, and on `viewDidChangeBackingProperties` or
  `windowDidChangeScreen:` updates the layer's `contentsScale`, the min size and the cursors and posts at once if
  size, N or mm changed (`ResizeCoalescer::post_now`, was `answered`). Display id for `CGDisplayScreenSize`:
  `deviceDescription()["NSScreenNumber"]`. Default geometry 1280x800 (`just macos-run` too). Not done: scroll
  deltas stay point-based; the mode list (`max_output`) is not recomputed when xpp or the screen changes.
- **Task 5** (`1894f263` core, `470c67cc` bridge, `92dff8ae` macOS; the macOS file is not compiled here, eiger
  pending): `Message::InternalClient(UnixStream)` runs the normal setup on a socketpair without authorization and
  sets `ClientState::internal`; `ServerState::counted_clients` feeds `note_client_count` and the last-client
  reset, and an internal client arms no reset and joins no XDMCP session. The bridge (`surface/clipboard/`) is a
  hand-written client (`conn.rs`, no new dependency) on its own thread, reconnecting 1 s after KillClient or a
  reset. Beyond decision 4, closer to ICCCM §2.6.2 / clipboard managers: it also answers `MULTIPLE` (failed pairs'
  targets set to None; no INCR inside MULTIPLE); when the X owner goes away (owner None, window destroy, client
  close) it re-owns CLIPBOARD with the host's text; when the host holds no text it releases CLIPBOARD; at startup
  it owns CLIPBOARD only if no X client does. Conversions use the XFIXES event's selection timestamp; the owner's
  SelectionNotify is matched on requestor/selection/target, not time. `TEXT` is answered as `STRING` when Latin-1
  holds the text, else `UTF8_STRING`; `STRING` maps characters past U+00FF to `?`. INCR above
  `max_request_bytes − 24` (65535 units, no BIG-REQUESTS), stalled transfers dropped after 10 s.
  `GeneralPasteboard` calls NSPasteboard through `DispatchQueue::main().exec_sync` (not `ShellUi`, which is
  fire-and-forget). Tests run the core loop on the hardware-free KMS test backend (not the Vulkan headless
  surface), so they are Linux-only. The bridge window has no `WM_NAME`/`WM_COMMAND`, so `xlsclients` does not
  list it. Open: whether the main queue drains while the quit alert is modal (the bridge waits, it cannot
  deadlock); PRIMARY.
- **Task 7** (`app_bundle.rs` is compiled and tested on Linux; the `main` hookup is macOS-only, eiger pending):
  `app_bundle::prepare` runs first in `main`, before any thread. A bundle launch is detected from the executable's
  path (`*.app/Contents/MacOS/<bin>`), not from a missing tty: running the bundle's binary from Terminal then also
  logs to the file, but a tty test would send a `just macos-run … | tee` run there too. Bundle launches `dup2` stdout
  and stderr onto `~/Library/Logs/yserver.log` (truncated per launch), so Mesa's and the loader's messages land
  there as well. With neither `VK_DRIVER_FILES` nor `VK_ICD_FILENAMES` set, it sets `VK_DRIVER_FILES` to the first
  `share/vulkan/icd.d/kosmickrisp_mesa_icd.<arch>.json` under `brew --prefix` at build time (`build.rs`,
  `YSERVER_HOMEBREW_PREFIX`), `/opt/homebrew`, `/usr/local`. An environment variable, not an explicit path: the loader
  takes a manifest path only from the environment, and `VK_LUNARG_direct_driver_loading` takes a loaded driver
  (we would re-implement `library_path` resolution). The loader itself was already found by `load_entry`.
  Display: `LaunchOptions::pick_free_display` (set by a bundle launch, not argv) makes a launch without `:N` and
  `-displayfd` take the lowest display from `:1` whose lock and socket are free (`launch::autopick_locked`, locked
  unlike the `-displayfd` scan; `:0` left to XQuartz, as M1); an explicit `:N` still wins. The window title is
  `yserver DISPLAY=:N`. `just macos-bundle` writes `target/yserver.app` with an `Info.plist` (identifier
  `io.github.joske.yserver`, version from `cargo pkgid`, `LSMinimumSystemVersion` 26.0 as KosmicKrisp needs,
  `NSHighResolutionCapable`); no icon (there is no artwork in the repo) and no `codesign` beyond the linker's ad-hoc
  signature. README has a macOS section.
