# macOS port, M2: keyboard, pointer and cursor

**Branch:** plan on `feat/macos-m0`, on top of M1 (`86fa03d5`, verified on eiger: twm + clients draw, redraw after
occlusion, macOS clippy clean). Needs a codex review before code moves. Style and gate follow
`2026-10-14-macos-m0-m1.md`.

**Goal:** the M1 window is usable. Typing, the mouse, the scroll wheel/trackpad and the X cursor work in the rooted
window; `xterm`, `twm`, `xeyes` and `xev` behave as on KMS.

**Non-goals:** IME and dead-key composition beyond XKB/Compose, the clipboard, resize and the HiDPI policy (M3),
rootless and GLX (M4), relative-pointer/confinement for games, deriving the XKB layout from the macOS input source.

Tags: **[C]** = checked in code at `86fa03d5` (file:line). **[A]** = assumption about macOS/AppKit, verified on
eiger before the task that depends on it merges.

## Facts the plan rests on

- **The KMS input path is portable already** [C]. libinput events become `HostInputEvent`
  (`yserver-core/src/core_loop/message.rs:322`) on the input thread and go to the core as `Message::HostInput`
  (`:254`). The core's `dispatch_host_input` (`core_loop/run.rs:1455`) keeps the `InputInventory`, arms key repeat
  (`update_repeat_state`, `:4254`) and calls `Backend::on_host_input`, which is the portable
  `backend_devices_on_host_input` (`kms/render/backend/portable/devices.rs:408`): XKB cooking, `key_fanout`,
  `pointer_fanout`, floating slaves. Nothing on that path is KMS-only.
- **The wire format a producer must emit** [C]: keys = `HostInputEvent::Key(HostKeyEvent { keycode: evdev + 8,
  state: 0, .. })` (`input_thread.rs:108–129`; the backend cooks modifiers). Buttons = Linux `BTN_*` codes; the
  backend maps `0x110/0x111/0x112/0x113/0x114` → X 1/3/2/8/9 (`devices.rs:549–558`). Wheel = press+release pairs of
  `SYNTH_SCROLL_{UP,DOWN,LEFT,RIGHT}` (`message.rs:395–398`), made by the v120 accumulator `drain_scroll`
  (`input_thread.rs:214`, 120 per click; finger scroll at 8 v120 per px, `input/context.rs:940`). XI2 smooth-scroll
  motion is derived from those wheel buttons by the fanout when the slave has ScrollClasses
  (`pointer_fanout.rs:178–200, 2094`). `PointerScrollStop` = fingers lifted (`message.rs:363`).
- **Origins must be registered** [C]: `on_host_input` drops events from an origin with no live XI source
  (`devices.rs:422–441`). Sources appear through `HostInputEvent::DeviceAdded(DeviceInfo)` with an `InputSourceId`
  and `InputCapabilities` (`message.rs:17`, `xinput/registry.rs:27–35`). The registry stores
  `device_node: Some(..)` unconditionally (`xinput/registry.rs:116,174`).
- **Timestamps** [C]: KMS stamps events with `crate::clock::server_time_ms()` on receipt, against the
  `START` instant pinned by `clock::init(state.start_instant)` (`clock.rs`; called in `surface/server.rs` `serve`
  before `link.connect`). A `server_time_ms()` call before `init` silently pins `START` to "now" and skews every
  later timestamp.
- **Key repeat is the core's, but its rate is hard-coded** [C]. `fire_pending_repeats` (`run.rs:4315`) uses
  `REPEAT_INITIAL_DELAY = 660 ms` and `REPEAT_PERIOD = 40 ms` (`run.rs:2032–2036`). XKB GetControls reports a
  different fixed 500/33 (`kms/xkb_desc/reply.rs:730–731`), and XKB SetControls (minor 7) is swallowed with no effect
  (`portable/keyboard.rs:842`). So `xset r rate` changes nothing on KMS today either.
- **Startup keymap** [C]: `resolve_startup_rmlvo` (`kms/core.rs:331`): `XKB_DEFAULT_*` env, then `-layout`,
  default evdev/pc105/us. In that keymap Super_L is on Mod4 and Alt/Meta on Mod1 (`kms/xkb.rs:958`).
- **Cursor** [C]: every cursor record keeps premultiplied BGRA bytes on the CPU (`yserver-render/.../cursor.rs:59`).
  The scene paints a SW cursor unless `CursorPlaneSink::fits` (`scene/build.rs:552`); in HW mode it calls `upload`
  (bytes + version) and `show(hot, x, y)` when the frame retires (`scene/cursor.rs:446–527`), and pointer motion
  goes to `Platform::move_hw_cursor`, whose default wakes the compositor (`seam.rs:81`). `SurfacePlatform` returns
  `fits = false` today (`surface/platform.rs:237`). `XWarpPointer` reaches `backend_pointer_warp_pointer_root`
  (`portable/pointer.rs:847`); no platform hook sees it.
- **M1 shell** [C]: the view is a plain `NSView` hosting the `CAMetalLayer` (`macos/window.rs`); there is no view
  subclass, no tracking area, and the main menu's Quit has key equivalent `q` (`macos/app.rs` `install_menu`).
  `ShellLink` (`surface/link.rs`) carries `ShellEvent`s to `on_platform_event` and holds the `CoreSender`.

## Decisions

### 1. Every key goes to X; Cmd is Super (maintainer, binding)

- A local event monitor (`NSEvent addLocalMonitorForEventsMatchingMask:` keyDown | keyUp | flagsChanged) takes every
  key event the app receives, hands it to the translator and returns nil. Nothing reaches `performKeyEquivalent:`,
  the menu, `interpretKeyEvents:` or the responder chain: no menu shortcuts, no beeps, no text input system. It also
  avoids AppKit's swallowed keyUp while Cmd is held, which a `keyUp:` override would miss. Local monitors need no
  Accessibility or Input Monitoring permission [A].
- Left Cmd → `KEY_LEFTMETA` (X 133, Super_L), right Cmd → `KEY_RIGHTMETA` (134, Super_R): Mod4, exactly like the
  PC Windows key. Option → `KEY_LEFTALT`/`KEY_RIGHTALT` (64/108): Mod1, which carries Meta in the evdev keymap.
  This is the Linux-on-Mac mapping (hid-apple), so X clients, WMs and users' muscle memory from Asahi agree. Who wants
  Cmd as Alt sets `XKB_DEFAULT_OPTIONS=altwin:swap_alt_win` or runs `setxkbmap`; no yserver flag.
- What macOS takes before the app sees it stays with macOS: Cmd-Tab, Cmd-Space, Mission Control, Cmd-Shift-3/4/5,
  Ctrl-arrows (Spaces), the media and brightness keys. Documented, not fought.
- Ctrl-click is not turned into a right click: the X client gets Control + Button 1.

### 2. Quitting: close button, a menu item without a shortcut, SIGTERM (maintainer, binding)

- Cmd-Q does **not** close the server; it reaches X as Super+Q like every other Cmd combination.
- The window's close button is the primary way to quit (M1 path: last window closed → terminate → `TerminateLater`
  → core shutdown).
- The app menu keeps "Quit yserver" with an empty key equivalent; the dock's Quit also works. Both go through the
  same `applicationShouldTerminate:`. No other shortcut: with decision 1's monitor, none could fire anyway.
- SIGTERM/SIGINT as in M1. The KMS emergency hotkeys (Ctrl-Alt-Backspace zap, Ctrl-Alt-Enter/F12 dumps) are not
  wired on macOS in M2.

### 3. Key repeat is server-side (maintainer, binding)

- keyDown with `isARepeat` is dropped in the translator. The core's repeat timer repeats the held key as on KMS
  (`update_repeat_state` also ignores a duplicate press, so a leaked repeat is harmless).
- Precondition, task M2.0: the timer must follow the XKB controls, so `xset r rate` works. That is an OS-neutral
  fix that also changes KMS (see Facts); it lands on master as its own branch and PR before M2.

### 4. Keycodes: one static table kVK → evdev, checked against the headers

- Source of the left column: `kVK_*` in HIToolbox `Events.h` (Carbon.framework, macOS SDK). Right column: `KEY_*`
  in Linux `include/uapi/linux/input-event-codes.h`. The pairing is by key identity, with
  `drivers/hid/hid-input.c` (`hid_keyboard[]`) as the authority where a key is named differently (JIS, Help,
  Clear). X keycode = evdev + 8. **No constant is typed from memory:** the implementer copies both columns from the
  headers (SDK on eiger, kernel tree), cites header + line in the module doc, and a test pins at least these
  entries, each re-checked against the headers before the test is written (values below are the expected ones,
  not yet the oracle):

  | kVK | evdev | X | | kVK | evdev | X |
  |---|---|---|---|---|---|---|
  | `ANSI_A` 0x00 | `KEY_A` 30 | 38 | | `CapsLock` 0x39 | `KEY_CAPSLOCK` 58 | 66 |
  | `Return` 0x24 | `KEY_ENTER` 28 | 36 | | `LeftArrow` 0x7B | `KEY_LEFT` 105 | 113 |
  | `Escape` 0x35 | `KEY_ESC` 1 | 9 | | `ANSI_KeypadEnter` 0x4C | `KEY_KPENTER` 96 | 104 |
  | `Command` 0x37 | `KEY_LEFTMETA` 125 | 133 | | `JIS_Yen` 0x5D | `KEY_YEN` 124 | 132 |
  | `RightCommand` 0x36 | `KEY_RIGHTMETA` 126 | 134 | | `ISO_Section` 0x0A | `KEY_102ND` 86 | 94 |

- A second test checks the whole table: injective, every X keycode in 8..=255, no entry for `kVK_Function` (Fn has
  no X keycode: `KEY_FN` is 0x1d0).
- Keys without an evdev peer (Fn) are dropped. Fn combinations that the keyboard turns into other keys (Fn-Left →
  Home, Fn-Delete → ForwardDelete) arrive as those keys [A].
- **ISO keyboards:** Apple ISO hardware reports the key left of `1` as `ISO_Section` and the key right of left
  Shift as `ANSI_Grave`, the reverse of the PC position (hid-apple's `iso_layout`). M2 swaps the two when
  `KBGetLayoutType(LMGetKbdType()) == kKeyboardISO` [A], so pc105 puts `<>` and `` ` `` where they are printed.
- **JIS:** `JIS_Yen` → `KEY_YEN`, `JIS_Underscore` → `KEY_RO`, `JIS_KeypadComma` → `KEY_KPJPCOMMA`,
  `JIS_Kana`/`JIS_Eisu` → `KEY_HANGEUL`/`KEY_HANJA` (HID LANG1/LANG2 in `hid-input.c`). A `jp` layout is the user's
  `-layout jp`.
- `ANSI_KeypadClear` → `KEY_NUMLOCK` (HID Keypad NumLock&Clear). Num Lock starts **locked on** on macOS, so the
  keypad types digits as Mac users expect (Clear toggles it).

### 5. Modifiers and Caps Lock

- flagsChanged carries the key's kVK. Press or release is read from that key's device-dependent bit in
  `modifierFlags` (`NX_DEVICEL/RSHIFTKEYMASK`, `…CTLKEYMASK`, `…ALTKEYMASK`, `…CMDKEYMASK` in IOKit
  `hidsystem/IOLLEvent.h`, cited and pinned like decision 4), not by toggling, so a lost event cannot invert a
  modifier.
- Caps Lock: macOS sends one flagsChanged when the lock turns on and one when it turns off, with no release. Each
  becomes a full press+release of `KEY_CAPSLOCK`, which toggles the X lock exactly once.
- Lock sync: when the window becomes key, the shell posts `ShellEvent::LockState { caps }` (from
  `NSEvent.modifierFlags`). The core compares it with the XKB locked Lock modifier and, if they differ, injects a
  `KEY_CAPSLOCK` press+release from the macOS keyboard origin. The Caps Lock LED stays macOS's
  (`set_keyboard_leds` keeps the no-op default).
- The translator keeps the set of keys it reported down. A release for a key not in the set is dropped (Cmd-Tab
  back: Cmd's release arrives without its press).

### 6. Focus loss releases everything

On `windowDidResignKey:` (Cmd-Tab, click elsewhere, minimise) the translator emits a release for every key it holds
down, in reverse press order, then a release for every held mouse button. The core's repeat stops on the release.
Nothing re-presses on `becomeKey`; only the lock sync of decision 5 runs.

### 7. Pointer

- A view subclass (`YserverView`, `acceptsFirstResponder = YES`, `acceptsFirstMouse: = NO`); the window sets
  `acceptsMouseMovedEvents = YES` (it defaults to NO, and without it no `mouseMoved:` arrives).
- Two `NSTrackingArea`s: motion `MouseMoved | MouseEnteredAndExited | ActiveAlways | InVisibleRect`, so `xeyes`
  follows even when the window is not key; and a separate cursor area `CursorUpdate | ActiveInKeyWindow |
  InVisibleRect`, because AppKit does not send `cursorUpdate:` for a `CursorUpdate` area with `ActiveAlways`.
- Position: `convertPoint:fromView:nil` of `locationInWindow` → points, origin bottom-left. X pixel
  `x = floor(px * scale)`, `y = floor((view_height_pt - py) * scale)`, clamped to `0..w-1`, `0..h-1`.
  `scale` = the window's `backingScaleFactor`, read per event batch on the main thread (M1's HiDPI rule: X pixels =
  backing pixels). Sent as absolute `PointerMotion { relative: false }`, `dx/dy` = NSEvent `deltaX/deltaY` × scale,
  rounded (RawMotion), `motion_delta: None`.
- Dragging outside the window keeps reporting (AppKit captures the mouse), clamped to the edge. Leaving the window
  without a button held stops motion; the X pointer stays where it left. X crossing events stay internal to X.
- Buttons by `buttonNumber`: 0/1/2 → `BTN_LEFT/RIGHT/MIDDLE`, 3 → `BTN_SIDE` (X 8), 4 → `BTN_EXTRA` (X 9), higher
  dropped.

### 8. Scrolling: the KMS model, fed from NSEvent

- One v120 accumulator for both platforms: `drain_scroll` moves out of the KMS-only `input_thread.rs` into an
  unconditional `src/scroll_v120.rs` (pure move, its tests move with it).
- `hasPreciseScrollingDeltas` (trackpad, Magic Mouse): `scrollingDeltaX/Y` points × 8 v120 per point, the KMS
  finger-scroll factor (15 points per click). Otherwise (wheel): `scrollingDeltaY` lines × 120. Sign: the deltas
  already include the user's natural-scrolling setting, which is honoured; positive `scrollingDeltaY` → button 4.
- `phase == Ended` or `Cancelled` (fingers lifted) → `PointerScrollStop` and the accumulator is reset, as libinput's
  stop. Momentum events (`momentumPhase != None`) are forwarded as ordinary deltas, so inertia feels native; their
  end resets the accumulator but sends no second stop.
- XI2 smooth scroll comes from the existing wheel emulation (whole clicks). Fractional smooth valuators would be a
  core change for both platforms; not M2.

### 9. Cursor: `NSCursor` through the existing `CursorPlaneSink`

Options:
- **(a) SW cursor in the scene** (M1 today) + `[NSCursor hide]` over the view. Zero new seam, but every pointer
  motion composes and copies a frame (the M1 copy is about 25 MB at 3200×2000), the cursor lags by a frame or
  more on FIFO, and rootless (M4) has no full-screen scene to paint into.
- **(b) `NSCursor` from the X cursor image.** `SurfacePlatform`'s `CursorPlaneSink` says `fits = true`; `upload`
  stores the premultiplied BGRA + version, `show` records the hotspot, `hide` selects an empty cursor. The core sends
  `ShellUi::set_cursor(Some(Arc<image>, hot) | None)` when the bound version or visibility changes. The main thread
  builds an `NSBitmapImageRep` (premultiplied BGRA) in an `NSImage` of `px / scale` points, caches the `NSCursor`
  by version, and `set`s it from `cursorUpdate:` and whenever it changes while the pointer is inside.
  `move_hw_cursor` is overridden to do nothing: motion never wakes the compositor.

**Recommendation: (b).** Zero-latency cursor (the window server draws it), no compose per motion, and it is the only
option that works unchanged for rootless. It uses the existing seam; the scene's HW-cursor bookkeeping (version,
hide/show on retire) is the one KMS already exercises.

Consequences:
- `XWarpPointer`/`warp_pointer_root` gets a new hook `Platform::host_pointer_warped(b, x, y)` (KMS: no-op). The
  surface platform sends `ShellUi::warp_pointer(x, y)`, done on the main thread with `CGWarpMouseCursorPosition`
  (global coords, top-left origin) followed by `CGAssociateMouseAndMouseCursorPosition(true)` to avoid the
  post-warp freeze [A]; only while the window is key and the pointer is inside.
- A cursor change applies when the scene retires a frame. That holds on KMS; on the surface platform a cursor-only
  change must still compose a (cheap, undamaged) frame, checked by a test (M2.4).
- Pointer confinement (grabs with `confine_to`) is enforced X-side; the host cursor may leave the confine area
  until the next warp. Accepted for M2.

### 10. Devices: two slaves under the existing masters

- The shell registers two sources through `DeviceAdded`: `InputSourceId(1)` "macOS keyboard"
  (`keyboard: true`) and `InputSourceId(2)` "macOS pointer" (`pointer: true`), vendor/product 0,
  `is_touchpad: false` (gestures are not raw touch), default `LibinputConfigSnapshot` (no libinput properties).
  `xinput list` then shows them as slaves of Virtual core keyboard/pointer next to the XTEST devices.
- `device_node`/`sysname` are empty, and the registry stores `device_node: None` for an empty string, so no
  bogus "Device Node" property is published (small core change; the KMS path always has a node).
- The constants and both `DeviceInfo`s live in the OS-neutral translator module.

### 11. Threading and timestamps

- Events are captured on the AppKit main thread, translated there (cheap, table lookups) and sent as
  `Message::HostInput` through the `CoreSender` the `ShellLink` already holds: new `ShellLink::send_input(&[ev])`.
  This is the KMS channel, so inventory, repeat and XI-config handling are the same code. `ShellEvent`s (lock
  sync) keep the `PlatformEvent` path.
- `ShellLink::connect` sends the two `DeviceAdded` before it stores the sender, under the same lock that
  `send_input` takes, so no input can precede its device. Input before `connect` is dropped (no window input
  before the core runs).
- Timestamps: `clock::server_time_ms()` on capture, the KMS rule. It is called only once connected, which is
  after `clock::init` (Facts). `NSEvent.timestamp` (uptime seconds) is not used: it would need a second epoch
  mapping and AppKit does not reorder events.

### 12. Code placement

- `src/surface/input/` (OS-neutral, compiled where `surface/` is, so Linux CI runs it): `keymap.rs` (the kVK table;
  kVK numbers are data, not an AppKit import, so layout rule 4 holds), `translate.rs` (the state machine: held keys,
  modifier bits, caps toggle, repeat drop, focus release, buttons), `pointer.rs` (coordinate transform, scroll
  front-end), `devices.rs`. Inputs are plain structs (`RawKey { kvk, kind, flags, is_repeat }`,
  `RawPointer { x_pt, y_pt, view_h_pt, scale, … }`); outputs are `HostInputEvent`s.
- `src/scroll_v120.rs`: the shared accumulator (decision 8).
- `src/macos/{view,input,cursor}.rs`: the view subclass, the event monitor and NSEvent → `Raw*` extraction, NSCursor
  construction and warp. Thin; checked on eiger.

## Gate (every task; **G**)

G of the M0/M1 plan (nightly fmt; clippy `--all-targets -D warnings` in the three configs; workspace tests; the
lavapipe `--ignored` count equal or higher; `tools/check-layout.sh`; FreeBSD CI on the PR), plus the emulated
non-KMS Linux clippy/test run from the M0.6 notes, plus on eiger `cargo clippy --workspace --all-targets -- -D
warnings` and `cargo test --workspace` for tasks touching `src/macos/`. Every new test is seen failing with the
feature stubbed out before it counts.

## Tasks

**M2.0 XKB repeat controls (own branch off master, PR, before M2 merges).**
- **Oracle first:** read Xorg's defaults and handling (`../xserver` `xkb/xkbInit.c` default delay/interval,
  `xkb/xkb.c` `ProcXkbSetControls`, `xkb/xkbAccessX.c` repeat timer) and `xset`'s `r rate` path
  (`XkbSetAutoRepeatRate`); record a measured Xvfb GetControls reply (delay, interval) as testdata next to
  `xorg-per-key-repeat.txt`.
- **Steps:** repeat delay/interval become core state (`ServerState`, Xorg defaults); SetControls (minor 7) parses
  the `XkbRepeatKeysMask`/`changeControls` fields per `XKBproto.h` and updates them, with the ControlsNotify Xorg
  sends; GetControls reports them; `update_repeat_state`/`fire_pending_repeats` read them instead of the consts.
- **Tests:** GetControls defaults equal the Xvfb testdata; SetControls(delay 200, interval 20) → GetControls
  reflects it and a held key's first repeat fires at 200 ms then every 20 ms (fake clock); ControlsNotify reaches
  a selecting client.
- **Acceptance:** G; HW smoke on silence: `xset r rate 200 50` changes repeat in xterm.

**M2.1 Translator (OS-neutral).**
- **Files:** `src/surface/input/*`, `src/scroll_v120.rs`, `input_thread.rs` (accumulator move only).
- **Steps:** decisions 4–8, 10. Header citations in `keymap.rs` before any test.
- **Tests (unit, Linux):** the pinned keycode entries and table properties; the ISO swap; shift press/release from
  device bits (left and right independent); caps on/off → two press+release pairs; repeat keyDown dropped; release
  of an unknown key dropped; focus loss releases keys in reverse order and held buttons; Cmd-Tab sequence leaves
  nothing held; coordinates at scale 1 and 2, y flip, clamp at both edges and outside; buttons 0–4 and 5 dropped;
  scroll: 3 wheel lines → 3 clicks, 7.5 + 7.5 precise points → 1 click, stop resets, momentum end resets without a
  stop; the moved `drain_scroll` tests unchanged.
- **Acceptance:** G.

**M2.2 Input channel and devices.**
- **Files:** `surface/link.rs`, `surface/server.rs`, `xinput/registry.rs` (empty node → `None`).
- **Steps:** `send_input`, device announcement in `connect`, drop-before-connect (decision 11).
- **Tests:** link: announcements precede input; nothing is sent before `connect`; a disconnected link drops.
  Lavapipe (`--ignored`), headless `XBackend<SurfacePlatform>`: after the announcements `XIQueryDevice` lists both
  slaves under masters 2/3 with no "Device Node" property; translated `a` press/release reaches a client window as
  KeyPress/KeyRelease keycode 38; Shift+a gives state ShiftMask; a scroll click gives ButtonPress 4 and an XI2
  smooth-scroll Motion to an XI2 selector; a held key repeats from the macOS origin and stops on a focus-loss
  release; the lock sync injects Caps only when the states differ.
- **Acceptance:** G.

**M2.3 AppKit glue.**
- **Files:** `macos/{view,input}.rs`, `macos/window.rs` (view subclass), `macos/app.rs` (monitor, resign/become
  key, menu Quit without key equivalent).
- **Steps:** decisions 1, 2, 5–7. `ShellEvent::LockState` on `windowDidBecomeKey:`.
- **[A] checks on eiger first (an event logger build):** local monitor sees Cmd-Q, Cmd-H, Cmd-W, Cmd-` and keyUp
  under Cmd; flagsChanged device bits per side; Caps Lock event shape (including the macOS hold-to-type delay);
  Fn-Left/Fn-Delete keycodes; ISO detection; trackpad phases.
- **Acceptance:** eiger: the acceptance list below minus the cursor items; G.

**M2.4 Cursor.**
- **Files:** `surface/platform.rs` (sink + `move_hw_cursor` + warp hook), `seam.rs` (`host_pointer_warped`),
  `portable/pointer.rs` (call it), `surface/server.rs` (`ShellUi::set_cursor`, `warp_pointer`),
  `macos/cursor.rs`.
- **Tests (lavapipe, a recording `ShellUi`):** the root cursor is set once at startup; `XDefineCursor` on the
  window under the pointer → one `set_cursor` with the new version and hotspot, also with no other damage;
  XFixes HideCursor → `set_cursor(None)`, ShowCursor restores; 100 motions → no composed frame and no `set_cursor`;
  `XWarpPointer` → one `warp_pointer` at the X position; an animated cursor steps versions.
- **Acceptance:** eiger: xterm I-beam over xterm, twm's cursors over frames, `xsetroot -cursor_name watch`; HiDPI
  cursor is 1:1 X pixels; G.

**M2.5 Acceptance + docs.**
- `docs/status.md` (macOS input row), this plan's implementation notes.
- **Acceptance on eiger (jos observes):**
  - xterm: US typing, Shift, Ctrl-C interrupts `sleep 100`, Caps Lock on/off with the macOS LED in sync after
    Cmd-Tab away, toggling, and back.
  - `xev`: keycodes/keysyms for `a` 38, Return 36, Escape 9, BackSpace 22, Delete (Fn-Delete) 119, Left 113, F1 67,
    Caps 66, Shift_L 50, Control_L 37, Alt_L 64, Alt_R 108, Super_L 133, Super_R 134; Cmd-Q shows Super_L + q and
    the server keeps running.
  - No stuck key or button after Cmd-Tab away and back, and after clicking another app with a key held.
  - Key repeat in xterm follows `xset r rate 200 50` and `xset r off`.
  - twm: move and raise windows with the mouse; middle/right menus; scroll in xterm (wheel mouse and trackpad);
    xeyes follows the pointer with the window not key; buttons 8/9 in xev from a 5-button mouse.
  - Quit: close button, menu Quit, SIGTERM — all clean as in M1.

## Risks

1. **Hidden AppKit key handling** [A]: some combinations may never reach a local monitor (secure input fields of
   other apps, Ctrl-arrows for Spaces, Cmd-Ctrl-Q lock screen). M2.3's logger lists what arrives; the rest is
   documented as decision 1's system list.
2. **Caps Lock timing**: macOS delays Caps-on for a short press and may use Caps to switch input sources; the X lock
   follows macOS's lock events, not the physical key. The lock sync covers drift on focus changes only.
3. **Cursor coupled to frame retirement** (decision 9): a cursor change on an occluded/minimised window waits for the
   next visible frame. Harmless (no cursor is visible then); the M2.4 tests pin the visible case.
4. **Warp and confinement** diverge from Xorg while the host pointer is outside X's confine area. Games and
   relative-mouse apps (pointer lock) are out of scope.
5. **Trackpad scroll feel**: 15 points per click is the KMS libinput factor, unmeasured on macOS; tuned on eiger
   by jos, value recorded in the implementation notes.
6. **M2.0 changes KMS behaviour** (GetControls reports different numbers, repeat follows SetControls). Gated by the
   Xvfb-measured testdata and the silence smoke; merged separately so it can be bisected.
7. **Server reset (`-reset`)**: the inventory is process-lifetime and devices are re-published on a new generation
   [C to verify in M2.2: `core_loop/reset.rs`]; a test resets once and checks both slaves are back.

## Review round 1 (codex) — resolved

1. `acceptsMouseMovedEvents = YES` on the window; the tracking area alone does not deliver motion.
2. Separate cursor-update tracking area (`ActiveInKeyWindow`); `CursorUpdate` + `ActiveAlways` gets no `cursorUpdate:`.
3. Num Lock starts locked on macOS.
4. Agreed: M2.0 (repeat fix) as its own PR to master; `NSCursor`; `pc105` + `us` with the ISO key swap.

## Open questions (maintainer / codex)

1. M2.0 changes XKB GetControls/SetControls on master for all platforms. OK to land it first as its own PR (it fixes
   `xset r rate` on Linux too), or keep it inside the M2 branch?
2. Cursor (b) `NSCursor` over (a) SW cursor: agreed? It needs the cursor-only-change frame and a new warp hook on
   the seam.
3. Default XKB model on macOS: `pc105` + `us` like Linux (M2 proposal), or pick `applealu_ansi/iso/jis` from
   `KBGetLayoutType` already? Deriving the layout from the macOS input source is proposed for M3.
4. ISO swap of `ISO_Section` ↔ `ANSI_Grave` (hid-apple's default behaviour): on by default for ISO keyboards, or
   off with a flag?
5. Trackpad momentum: forward it (proposal, native feel) or drop it after the fingers lift (closer to libinput)?
6. Num Lock: `KeypadClear` → `KEY_NUMLOCK` with Num Lock starting off (Linux default) means keypad digits are
   navigation keys until Clear is pressed; start with Num Lock locked on macOS instead?
7. `acceptsFirstMouse: NO` (macOS convention: the first click only activates) vs `YES` (the click also reaches X,
   like XQuartz's click-through option)?

## Implementation notes

- **M2.1 keycode sources:** Apple's `Events.h` is not on the Linux build host and no crate in the registry carries the
  `kVK_*` constants. Every value was cross-checked against pynput 1.8.2 (`_util/darwin_vks.py`, `keyboard/_darwin.py`)
  and/or XQuartz `quartzKeyboard.c` (xorg-server 21.1.24). No second source for `ISO_Section` 0x0A,
  `ANSI_KeypadClear` 0x47, `JIS_Yen` 0x5D, `JIS_Underscore` 0x5E, `JIS_KeypadComma` 0x5F, `JIS_Eisu` 0x66,
  `JIS_Kana` 0x68, `Help` 0x72, `Function` 0x3F (marked `*` in `keymap.rs`), nor for the IOLLEvent.h
  `NX_DEVICE*KEYMASK` bits: all await a check against the SDK on eiger. The pinned-entry test holds only
  cross-checked kVK values, so `JIS_Yen` and `ISO_Section` from decision 4's table are not pinned. `KEY_*` values are
  copied from `/usr/include/linux/input-event-codes.h` with their line numbers.
- **M2.1 pairing additions:** `Help` → `KEY_INSERT` (the Apple key in the PC Insert position, HID Insert),
  `ContextualMenu` 0x6E → `KEY_COMPOSE`; the Apple JIS keypad comma → `KEY_KPJPCOMMA` agrees with xkeyboard-config
  (`macintosh_vndr/jp`: `<JPCM>`). Volume/mute kVKs are left out (macOS consumes those keys).
- **M2.1 modifiers:** a sided modifier is down when both its device bit and its device-independent bit are set, so a
  wrong device-bit constant shows as a dead modifier, never as a stuck one.
- **M2.1 Num Lock:** locked by a `KEY_NUMLOCK` press+release that follows the two `DeviceAdded` in the shell's
  announcements (`devices::announcements`), not by seeding the XKB state. A server reset rebuilds the XKB state
  with Num Lock off.
- **M2.1 scroll:** `Scroller` keeps the sub-v120 fraction per axis before the integer `ScrollV120` accumulator, so
  small trackpad deltas are not lost to rounding. `ScrollV120` (`src/scroll_v120.rs`) is the moved
  `drain_scroll` state; its four tests moved unchanged apart from the receiver.
- **M2.2 channel:** `ShellLink::send_input(build)` takes a closure: it is called with the server time only once the
  core connected, under the sender lock, so nothing stamps `server_time_ms` before `clock::init` and a shell's
  translator state is not touched by input that is dropped. `connect` sends `devices::announcements` first.
- **M2.2 lock sync:** `ShellEvent::LockState` is applied in `on_platform_event`; the Caps press+release goes back
  through the core channel (`Message::HostInput`, keyboard origin) rather than straight into `on_host_input`, so it
  queues behind input already in flight.
- **M2.2 tests:** the lavapipe tests check the registry (both slaves attached to masters 3/2, `device_node` `None`,
  no "Device Node" property), core KeyPress/KeyRelease with Shift and Num Lock state to a root-selecting client,
  wheel → ButtonPress 4, the core repeat armed by a held key and cleared by the focus-loss release, and the lock
  sync. The XI2 smooth-scroll motion is the existing fanout path (`pointer_fanout`) and is not re-tested. Risk 7:
  the announcements pass `dispatch_host_input`, so the process-lifetime inventory replays them after a reset
  (`core_loop/reset.rs:420–423`); not exercised by a test here.
- **M2.3 glue:** `macos/input.rs` (`ShellInput`: NSEvent → `RawKey`/`RawPointer`/`RawScroll`, the key monitor, the
  Carbon ISO query), `macos/view.rs` (`YserverView`, the layer-hosting content view with the motion tracking area).
  The window delegate sends `focus_lost`/`focus_gained` on resign/become key. New macOS dependency `block2 =0.6.2`
  (already in the lock through the app-kit bindings) for the monitor's block. `RUST_LOG=yserver::macos=debug` logs
  every key event (kVK, kind, flags, repeat) — the event logger for the [A] checks; `trace` adds scroll phases.
  Nothing in `src/macos/` was compiled on the Linux host (no Apple target installed).
- **M2.3 pointer:** a button event first sends a motion to its own position, so a click after the pointer re-entered
  without moving lands where it is. `acceptsFirstMouse:` is NO: the activating click is not sent; a release without
  its press is dropped by the translator.
- **M2.4 seam:** `ShellUi` moved from `surface/server.rs` (non-KMS only) to `surface/ui.rs`, so the platform and the
  Linux tests see it; it is `Send + Sync` and shared as `Arc` (`WindowSpec.ui`, `SurfaceBackendState.ui`).
  `CursorImage` carries the record version, size, hotspot and the premultiplied BGRA bytes; `rgba()` swizzles
  for `NSBitmapImageRep` (default layout: RGBA, alpha last, premultiplied).
- **M2.4 sink:** `SurfacePlatform` is a cursor plane on every surface (`fits` for any size). The sink only records
  image/hotspot/visibility; `flush_host_cursor` (after `maybe_composite` and after retirement) sends
  `ShellUi::set_cursor` when `(version, hotspot)` or visibility changed. Nothing is sent before the first shown
  cursor, so the shell keeps its own arrow until then. `move_hw_cursor` is a no-op and
  `Platform::host_pointer_warped` (default no-op; called at the end of `warp_pointer_root` with the clamped
  position) sends `ShellUi::warp_pointer`.
- **M2.4 animation:** the surface platform did not tick animated cursors (M0.6 listed it as KMS-only portable code).
  `maybe_composite` now calls `tick_cursor_animation` and `next_wakeup` includes `cursor_anim_deadline` (made
  `pub(crate)`) while a surface is visible.
- **M2.4 tests (lavapipe, recording `ShellUi`):** startup sends the root cursor once and never re-sends it; a
  DefineCursor on the root alone makes a frame due and sends one cursor with the new version and hotspot; XFixes
  hide/show (`set_cursor_hidden`) sends `None` then the same version; 100 shell motions make no frame due, present
  nothing and send nothing; a warp sends one clamped `warp_pointer`; an animated cursor sends a rising version per
  frame. Each was seen failing with its feature removed.
- **M2.4 shell:** `macos/cursor.rs` builds an `NSCursor` per `(version, hotspot)` (cache of 64) with an image of
  `px / backingScaleFactor` points, keeps the current one in a main-thread `thread_local`, `set`s it when the
  pointer is over the key window and from `cursorUpdate:` (second tracking area: `CursorUpdate | ActiveInKeyWindow
  | InVisibleRect`); hidden = a 1×1 transparent cursor. The warp converts to Quartz global coordinates
  (top-left of the primary screen) and is skipped unless the window is key and the pointer is over the view.
- **Emulated non-KMS build (as M0.6):** clippy `--all-targets -D warnings` (default and `xdmcp`) clean and
  `cargo test -p yserver --all-targets` green (222 passed); it caught `ScrollV120::is_empty` being test-only dead
  code without KMS (fixed by a reset test).
- **M2.5 numbers (silence, lavapipe, `cargo test --workspace --all-targets --features xdmcp --no-fail-fast --
  --ignored`):** base `1862b339` 343 passed / 1 failed, branch 354 passed / 1 failed (+11: five input, six cursor
  tests). The failure is the same on both: `render_picture::pixmap_pool_eviction_and_idle_trim_free_the_memory`
  ("the most recent return survives eviction", `render_picture.rs:972`), unrelated to M2.
- **M2.5 open for eiger:** compile `src/macos/` (`cargo clippy --workspace --all-targets -- -D warnings`), the [A]
  checks of M2.3 with `RUST_LOG=yserver::macos=debug`, the unverified kVK/NX constants, the ISO swap direction, the
  scroll sign and the 15-points-per-click trackpad factor, then the acceptance list above.
