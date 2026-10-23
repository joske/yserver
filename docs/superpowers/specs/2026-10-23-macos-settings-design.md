# yserver.app Settings window

Design spec · 2026-10-23

## Goal

A Settings window inside yserver.app (app menu → "Settings…", Cmd-,), like
XQuartz's Preferences, for the options a Mac user would otherwise have to pass
on the command line. Not a separate app.

## Today (master 9ada0910)

There is no settings layer. Everything comes from argv, from defaults that
depend on running from the bundle, or is hardcoded:

| Setting | Today | Changes at runtime? |
|---|---|---|
| Rootless vs window | `launch::shell_mode`: flag, else rootless when started from the bundle | no |
| Click emulation | always on: Option-click = button 2, Cmd-click = button 3 (`translate.rs:203`) | trivially |
| Cmd keys | every key goes to X, Cmd-Q included; the menu's Quit has no key equivalent (`macos/input.rs:85`, `app.rs:448`) | yes |
| Clipboard | CLIPBOARD ⇄ pasteboard, text only, always on; PRIMARY not bridged (`surface/clipboard`) | needs a bridge flag |
| Scale | `--scale auto\|1\|2`, fixed at startup | no |
| TCP / XDMCP | flags exist, but the bundle is built without `tcp-transport`/`xdmcp`, so it refuses them | no |
| Launch at login, DISPLAY | nothing | — |

XQuartz's defaults for the overlapping keys: fake buttons **off**, key
equivalents **on** (Cmd keys go to the Mac), `nolisten_tcp` off.

## Settings (v1)

Stored in `NSUserDefaults.standardUserDefaults` (domain = bundle id
`io.github.joske.yserver`). A command-line flag overrides the stored value for
that run and is not written back.

| Key | Values (default) | UI | Applies |
|---|---|---|---|
| `mode` | `rootless` \| `window` (`rootless`) | radio | next launch |
| `emulate_buttons` | bool (`true`) | checkbox "Option-click = middle, Command-click = right" | immediately |
| `key_equivalents` | bool (`true`) | checkbox "Command shortcuts go to yserver (Cmd-Q, Cmd-H, Cmd-,)" | immediately |
| `sync_clipboard` | bool (`true`) | checkbox "Sync the clipboard with the Mac pasteboard" | immediately |
| `scale` | `auto` \| `1` \| `2` (`auto`) | popup | next launch |
| launch at login | `SMAppService.mainApp.status` (not stored) | checkbox | immediately |

Defaults keep today's behaviour except `key_equivalents`: today Cmd-Q goes to
X, which makes Cmd-, unreachable and the app unquittable from the keyboard.
With key equivalents on, only the app's own menu shortcuts (Cmd-Q, Cmd-H,
Cmd-Alt-H, Cmd-,) are taken by AppKit; every other Cmd combination still goes
to X as Super, as today. Off restores today's "everything to X".

"Next launch" settings show "Takes effect when yserver restarts" and a
Restart button (see Restart below).

Saved settings apply only when yserver runs from the bundle. `just macos-run`,
`macos-rootless`, `--headless` and any non-bundle start use the built-in
defaults above (key equivalents on), not the saved values.

## Not in v1

- TCP listening and XDMCP. `ssh -X` needs neither: the ssh client reaches the
  local server through `DISPLAY`'s Unix socket. Building the bundle with
  `tcp-transport`/`xdmcp` stays possible for whoever wants direct network clients.
- PRIMARY bridging (decided 2026-10-15: leave for now).
- Exporting DISPLAY to the user session (follow-up). XQuartz does it with a
  LaunchAgent whose socket has `SecureSocketWithKey = DISPLAY`, so launchd sets
  DISPLAY for every process at login and starts XQuartz on first connect. Ours
  would compete with XQuartz for that variable, and needs Homebrew's libxcb to
  accept a path-style DISPLAY; `launchctl setenv DISPLAY` only reaches processes
  started afterwards. Its own design.
- Image/file clipboard.

## Mechanics

- **Menu:** the app menu gets About, Settings… (Cmd-,), Hide (Cmd-H), Hide
  Others (Cmd-Alt-H), Show All, Quit (Cmd-Q). Today it has only Quit.
- **Key routing:** the local NSEvent monitor currently swallows every key
  event. With `key_equivalents` on, a KeyDown with Command is offered once to
  `NSApp.mainMenu.performKeyEquivalent:`, which executes the item if it has one.
  Handled: the event is consumed and never reaches X. Not handled: it goes to X
  as today. The call is the dispatch, never a predicate followed by a second
  delivery. Off: nothing is offered to the menu.
- **Focus changes:** when an AppKit window of ours (Settings, an alert) becomes
  key, every key and modifier X believes is down is released in X, whether or
  not key equivalents are on, so no key sticks. The same on returning focus to
  an X window: X sees only what is pressed from then on.
- **Window:** a plain NSWindow (titled, closable, not resizable) built in code
  with NSStackView rows; no nib. One instance, brought to front on reopen.
- **Click emulation** is read when a press is translated. A press emulated as
  button 2 or 3 is released as that same button even if the setting changes
  while it is held.
- **Clipboard off:** both directions stop. The bridge stops polling the
  pasteboard, ignores X selection-owner changes and relinquishes CLIPBOARD
  ownership with `SetSelectionOwner(None)` at once. An INCR send already
  started is finished: its requestor holds a SelectionNotify of type INCR,
  which ICCCM §2.7.2 cannot take back, and the transfer runs on the
  requestor's property, independent of who owns the selection. A request
  not yet answered gets the normal refusal (SelectionNotify with property
  None); every request gets exactly one SelectionNotify. **Clipboard on:** the Mac pasteboard is taken as
  the current content (the bridge claims CLIPBOARD for it, as at startup); a
  change made in X while it was off is not pushed to the Mac until X sets
  CLIPBOARD again.
- **Launch at login:** the checkbox shows `SMAppService.mainApp.status`:
  enabled, not registered, or requires approval (then with a hint and a button
  that opens Login Items in System Settings). Toggling calls `register` or
  `unregister` and re-reads the status; an error is shown, not stored. A disable
  made in System Settings is respected, never undone.
- **Launch-time settings:** `launch::parse_args` starts from the stored values
  when running from the bundle and applies argv on top. A setting fixed by a
  command-line flag is shown disabled with "set by command line" and is not
  written back. The usage string gains the macOS flags.
- **Restart:** after the existing quit confirmation (clients connected), the
  server shuts down fully (clients disconnected, socket and lock file removed),
  then the app relaunches itself from its bundle URL with **no** arguments, so
  the saved values apply. Command-line overrides of the current run do not
  survive a restart; the note next to the Restart button says so when any are
  active.

## Tests

- Unit:
  - settings → `LaunchOptions` merge: stored value, flag override, flag not
    written back, non-bundle start ignores stored values;
  - key routing: a handled Cmd shortcut is dispatched once and not sent to X;
    unhandled Cmd keys and everything when off go to X;
  - focus to Settings releases held keys and modifiers in X;
  - click emulation turned off while an emulated button 3 is held releases
    button 3; the next press is button 1;
  - clipboard off during an INCR send: the send completes with no second
    SelectionNotify, and CLIPBOARD ownership is released; a request queued
    but unanswered when it goes off is refused exactly once;
  - restart with CLI overrides relaunches without them.
- eiger: each checkbox live, each next-launch setting after a restart, launch at
  login across a logout, quitting with Cmd-Q, Cmd-, opening Settings in both
  modes.
