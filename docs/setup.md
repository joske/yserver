# Setting up yserver

This guide covers installing yserver, giving it access to the hardware it
needs, and starting a session — from a display manager or directly from a
console.

Installed copies live at `$prefix/share/doc/yserver/setup.md`, alongside
the example configs referenced below.

## Packages

- **Arch** — `yserver` (tagged releases) and `yserver-git` (tracks
  `master`, maintained by a third party) on the AUR.
- **Fedora, Debian, Ubuntu, Alpine** — see <https://github.com/joske/yserver-packaging>.
- **Anything else** — build from source, below.

## Requirements

- A GPU with a working Vulkan driver. A software implementation such as
  lavapipe is refused by default; it is far too slow to be usable. The
  Fedora and Debian packages pull a driver in as a weak dependency; the
  Alpine and AUR ones deliberately do not, since the right one is
  hardware-specific — install it yourself there (`mesa-vulkan-ati` or
  `mesa-vulkan-intel` on Alpine, `vulkan-radeon` or `vulkan-intel` on Arch).
- Linux with atomic KMS/DRM, FreeBSD, or OpenBSD.
- Access to `/dev/dri/*` and `/dev/input/event*` — see
  [Device access](#device-access).
- For `--vrr all` on NVIDIA, Mesa's
  `/usr/share/drirc.d/00-mesa-defaults.conf` application list is required:
  install `mesa` on Arch/Alpine, `libgl1-mesa-dri` on Debian/Ubuntu,
  `mesa-dri-drivers` on Fedora, `mesa-dri` on FreeBSD, or `media-libs/mesa`
  on Gentoo.

## Build from source

A recent stable Rust toolchain plus:

### Arch

```sh
sudo pacman -S --needed just gcc libxshmfence libxkbcommon libinput shaderc systemd-libs fontconfig pkgconf mesa scdoc
```

### Ubuntu / Debian

```sh
sudo apt install just gcc libxshmfence-dev libxkbcommon-dev libinput-dev glslc libudev-dev libfontconfig-dev libgbm-dev scdoc
```

### Alpine

```sh
export RUSTFLAGS="-C target-feature=-crt-static"
apk add gcc musl-dev fontconfig-dev freetype-dev libxshmfence-dev libxkbcommon-dev libinput-dev shaderc mesa-dev scdoc
```

### FreeBSD

Run as root:

```sh
pkg install just pkgconf shaderc fontconfig freetype2 libudev-devd libinput \
  libxkbcommon libxshmfence libdrm mesa-libs vulkan-loader scdoc
```

Install a recent stable Rust toolchain too (`pkg install rust` provides the
FreeBSD-packaged toolchain).

For Intel or AMD GPUs, install `mesa-dri` for the Vulkan driver and
`drm-kmod` for the kernel driver, then configure the appropriate kernel
module as described in the
[FreeBSD graphics guide](https://docs.freebsd.org/en/books/handbook/x11/).
`mesa-libs` alone does not provide the Vulkan GPU driver. Use the appropriate
NVIDIA driver packages for NVIDIA hardware instead.

### OpenBSD

Install the X sets (Mesa's Vulkan drivers, libgbm, fontconfig and
libxshmfence come from there), then as root:

```sh
pkg_add rust git just shaderc vulkan-loader libinput-openbsd libudev-openbsd \
  libxkbcommon scdoc
```

rustc needs more memory than the default data size limit allows. Raise
`datasize-cur` (and `datasize-max` if it is lower) for your login class in
`/etc/login.conf`, e.g. `:datasize-cur=8G:` under `staff`, and log in
again. Run `cap_mkdb /etc/login.conf` first if `/etc/login.conf.db`
exists.

Then build and install:

```sh
cargo build --release --bin yserver
just man
sudo PREFIX=/usr/local just install
```

Or in one step, which builds, renders the man pages and installs to
`/usr/local` with `sudo`:

```sh
just install-local
```

### Optional build features

Two capabilities are compiled in only when asked for:

| Feature         | Enables                                                         |
| --------------- | --------------------------------------------------------------- |
| `tcp-transport` | `-listen tcp`, the TCP listener                                 |
| `xdmcp`         | `-query`, `-indirect` and `-broadcast`; implies `tcp-transport` |

```sh
cargo build --release --bin yserver --features tcp-transport
cargo build --release --bin yserver --features xdmcp
```

They are off for two different reasons. Listening on TCP widens the
server's exposure, so it should be an affirmative decision by whoever
builds the binary — a stronger position than the runtime `-nolisten tcp`
convention, because the capability is absent rather than merely switched
off. XDMCP is niche: most deployments never negotiate a session with a
display manager, and it is several thousand lines that would otherwise
ship unused.

A build without a feature does not silently ignore the options it covers.
It exits at startup with a message naming the feature. `-nolisten tcp`
stays accepted either way, so a display manager's argv needs no
adjusting.

`yserver --version` ends with the set that was compiled in, as
`features=[tcp-transport,xdmcp]`, `features=[tcp-transport]` or
`features=[]`.

`just install-local` builds with neither. To install a build that has
them, run the `cargo build` above in place of the plain one, then
`just man` and `just install` as shown earlier.

To run a session you also need `xauth` at runtime (`xorg-xauth`).
`starty` uses `mcookie` (`util-linux`) when present and reads
`/dev/urandom` otherwise.

Packagers: see [Packaging](#packaging).

## Device access

yserver has no seat manager. The server process opens the DRM and input
devices itself, so it needs read/write access to both. How it gets that
access depends on how it is started — a display-manager-launched server
runs as root and already has it; a user-launched one does not.

### Linux

Add your user to the `video` and `input` groups, then log out and back in
(group membership is established at login):

```sh
sudo usermod -aG video,input $USER
```

Alternatively grant the devices via seat ACLs. Confirm access with:

```sh
ls -l /dev/dri/ /dev/input/event0
```

### FreeBSD

Add your user to the groups owning `/dev/dri/*` and the input devices —
conventionally `video` and `operator`; check with `ls -l` as above. The
`systemctl` commands elsewhere in this guide do not apply.

Console takeover is implemented on FreeBSD `vt(4)` consoles too, so
`Ctrl-C` in an X client is shielded from the kernel's console signal
handling the same way it is on Linux VTs.

### OpenBSD

Log in on the first console, `ttyC0`: the default `/etc/fbtab` hands the
keyboard, the mouse and the first GPU (`/dev/dri/card0`,
`/dev/dri/renderD128`) to whoever logs in there. With more than one GPU,
add the other cards' nodes to that line.

Alternatively make the devices group-accessible to `wheel` (your user must
be a member):

```sh
chmod 660 /dev/wskbd* /dev/wsmouse* /dev/dri/*
```

## The X11 socket directory

X servers share `/tmp/.X11-unix`, which must be root-owned and sticky
(`1777`) so any user can bind a socket in it. yserver creates the
directory if it is missing but does not set its mode, so on a machine
where no other X server has ever run, a user-launched server leaves it
owned by that user — and other users then cannot start a display.

On systemd systems the installed `$prefix/lib/tmpfiles.d/yserver.conf`
handles this. Apply it without rebooting:

```sh
sudo systemd-tmpfiles --create
ls -ld /tmp/.X11-unix    # expect: drwxrwxrwt ... root root
```

On non-systemd Linux, arrange the equivalent in local startup:

```sh
mkdir -p /tmp/.X11-unix && chown root:root /tmp/.X11-unix && chmod 1777 /tmp/.X11-unix
```

On FreeBSD, the same but with the conventional group:

```sh
mkdir -p /tmp/.X11-unix && chown root:wheel /tmp/.X11-unix && chmod 1777 /tmp/.X11-unix
```

## Use with a display manager (lightdm)

lightdm can launch yserver as its X server for a graphical login. It is
the practical choice because its X server command is configurable — gdm
and sddm do not expose that.

Copy the installed example into place and restart lightdm **from a free
console**, not from inside the session you are about to replace:

```sh
sudo install -m644 /usr/local/share/doc/yserver/examples/lightdm-99-yserver.conf \
    /etc/lightdm/lightdm.conf.d/99-yserver.conf
sudo systemctl restart lightdm
```

On FreeBSD the drop-in goes in `/usr/local/etc/lightdm/lightdm.conf.d/`
and lightdm restarts with `service lightdm restart`. OpenBSD has no
lightdm package; its display manager is xenodm.

The greeter appears, you log in, and lightdm's PAM stack unlocks the login
keyring as usual.

The example already names the path yserver was installed to; if you move
the binary, update `xserver-command`. Do **not** point it at `starty` —
that is the console launcher, and it forks rather than execs, so the
readiness handshake would never reach lightdm.

## Use directly on a console (starty)

`starty` is the installed counterpart of `startx`. Switch to a free
console and run:

```sh
starty                 # runs ~/.xinitrc (or /etc/X11/xinit/xinitrc)
starty bspwm           # ...or a WM resolved via PATH, no xinitrc needed
starty bspwm -c ~/rc   # ...with arguments
starty -- --vrr        # pass --vrr to yserver with the xinitrc session
starty bspwm -- --vrr  # pass --vrr to yserver with bspwm
```

Server arguments go after `--`; arguments before it remain the client and its
arguments. `--vrr` is off by default. It enables VRR for windows that set
`_VARIABLE_REFRESH`, on outputs whose monitor reports VRR support. NVIDIA
proprietary clients do not set `_VARIABLE_REFRESH`; on NVIDIA (`nvidia-drm`),
`--vrr all` enables VRR for every directly scanned-out client window instead,
except applications that the NVIDIA or Mesa application lists, or the
client's environment, exclude.

### Direct scanout with a desktop compositor

Direct scanout, and therefore VRR, needs a compositing desktop to unredirect
fullscreen windows. On Cinnamon, set
`org.cinnamon.muffin unredirect-fullscreen-windows` to `true` in System
Settings → General → “disable compositing for full-screen windows”. This
setting is off by default; with it off, Muffin composites the game and the
monitor stays at its maximum refresh rate.

Cinnamon has a known limitation, measured on 2026-10-27 and also seen on
Xorg: a system-tray icon (for example Steam's, which is present whenever a
game is launched through Steam) creates an override-redirect window stacked
above the fullscreen game. Muffin then does not unredirect the game, so VRR
stays off ([upstream issue #12687](https://github.com/linuxmint/cinnamon/issues/12687)).
A desktop without a compositor, such as i3, is unaffected. Direct scanout is
per output, so a client covering exactly one monitor can scan out there while
other monitors keep composing at their own rates. A Present selecting another
monitor's CRTC is composed. A whole-screen window can use grouped direct
scanout when all outputs share the primary device and refresh rates within
0.5%. Tearing has one direct owner at a time and may belong to a fullscreen
client on one monitor while other monitors are on. A second client asking to
tear stays direct with synchronized flips until the current owner releases
tearing. A paused owner keeps tearing. VRR is per monitor: each output with a
capable connector uses VRR while its own fullscreen client scans out directly
and asks for it, so several monitors can use VRR at once; others keep their
fixed refresh.

It picks the lowest free display, mints a per-session
MIT-MAGIC-COOKIE-1 (a server copy plus an entry in `~/.Xauthority`),
waits for the socket, runs the session, and tears the server down on
exit. It refuses to run from a pseudo-terminal, so it cannot be started
over SSH or from a terminal inside another graphical session.

Logs:

| File                                 | Contents       |
| ------------------------------------ | -------------- |
| `~/.local/state/yserver/yserver.log` | server output  |
| `~/.local/state/yserver/session.log` | session output |

Raise server verbosity with `RUST_LOG=debug starty`.

From a source checkout you can also use `just startx`, which does the same
thing against the in-tree debug build.

## Remote clients over TCP

**You probably want `ssh -X` instead.** It needs nothing below: it proxies
through a UNIX socket on the remote host, so it never touches yserver's TCP
listener at all, and it encrypts the connection. The TCP listener exists for
XDMCP, where a display manager hands each session its own cookie.

TCP is off at runtime, as on a modern Xorg build, and it is not compiled in at
all unless the binary was built with the `tcp-transport` feature — see
[Optional build features](#optional-build-features). Without it, `-listen tcp`
exits at startup naming the feature. To enable the listener on a binary that
has it:

```sh
yserver :7 -auth /path/to/authority -listen tcp
```

The port is 6000 plus the display number, and the server binds `0.0.0.0` —
every interface, not only loopback. `-listen tcp` refuses to start without a
`-auth` file containing a usable MIT-MAGIC-COOKIE-1, because the alternative
would be an unauthenticated X server on the network.

**Over TCP the cookie is the entire boundary.** `xhost` entries never admit a
TCP client: `xhost +somehost` and even `xhost +` let no one in without the
cookie (Xorg would). `xhost` only applies to the local socket, where
`xhost +si:localuser:<user>` lets that user's clients in without a cookie. The
wire protocol is unencrypted, keystrokes included. Read the SECURITY section of
`yserver(1)` before exposing a display this way.

For a client on another machine, the cookie has to be keyed to how that client
names the display:

```sh
# on the server, read the cookie yserver is validating against
xauth -f /path/to/authority list

# on the client, key it to the address the client will connect to
xauth add 192.168.1.10:7 MIT-MAGIC-COOKIE-1 <cookie>
DISPLAY=192.168.1.10:7 xterm
```

Two traps worth knowing. `xauth add hostname:7` may store an **IPv6** entry if
the name resolves to IPv6 first, and yserver's listener is IPv4-only, so use
the IPv4 address literal on both sides. And a loopback connection is different
from a remote one: xtrans rewrites `127.0.0.1` to a local entry before looking
up the cookie, so an ordinary `:7` cookie works for `127.0.0.1:7` and tells you
nothing about whether a genuinely remote client will authenticate.

Some capabilities are unavailable to any TCP client, because they pass file
descriptors over the socket and that has no network equivalent: DRI3, MIT-SHM,
and Present's buffer import. As on Xorg, the extensions are still advertised
and the requests fail — DRI3 with `BadMatch`, MIT-SHM with `BadRequest` — so
clients fall back the same way they do against a remote Xorg. In practice that
means no GPU acceleration for remote clients.

## XDMCP: getting your session from a display manager

XDMCP inverts the usual arrangement. Instead of starting a session locally,
yserver asks a display manager — possibly on another machine — for one, and
the manager's greeter and session clients connect *back* to your display over
TCP. It is the thin-client model, and it is what `-listen tcp` exists for.

This needs a binary built with the `xdmcp` feature — see [Optional build
features](#optional-build-features). Without it, `-query`, `-indirect` and
`-broadcast` exit at startup naming the feature.

**The manager must speak XDMCP, and many no longer do.** GNOME's `gdm` has
removed support entirely. `lightdm` works:

```ini
# /etc/lightdm/lightdm.conf
[XDMCPServer]
enabled=true
port=177
```

Then, from a TTY on the machine that will *be* the display:

```sh
yserver :7 -query <manager-host> -listen tcp
```

or `just yserver-xdmcp-hw <manager-host>`, which does the same and captures a
log. `just` arguments are positional, so the manager comes first, then the log
level, then `1` for `-once`.

No `-auth` is needed or wanted: the manager sends a per-session cookie in its
reply, and while XDMCP is driving, that cookie is the **only** thing that
authorizes a TCP client — a cookie in a local file will not do, because it
does not belong to this session. `-query` also implies `-reset`, so logging
out erases the session and queries again for the next user.

Add `-once` to exit after one session instead of looping. That covers every
renewal path, including failing to reach the manager at all.

**Before trusting it on a network**, read the XDMCP section of `yserver(1)`.
There is no authentication of the manager and no encryption of the
connection; XDMCP assumes a trusted network, and that assumption is doing real
work.

## Key bindings

| Keys                           | Effect                                                          |
| ------------------------------ | --------------------------------------------------------------- |
| `Ctrl-Alt-Backspace`           | terminate the server, return to the console                     |
| `Ctrl-Alt-F1` … `Ctrl-Alt-F11` | switch to virtual console 1–11                                  |
| `Ctrl-Alt-Enter`               | write the current scanout to a PPM in the working directory     |
| `Ctrl-Alt-F12`                 | write every drawable's storage to PPMs in the working directory |

## Troubleshooting

**Permission denied on input or DRM devices.** Device access is not set
up, or you have not logged out and back in since adding the groups. See
[Device access](#device-access).

**"must be run from a TTY".** `starty` was run from a pseudo-terminal.
Switch to a real console.

**No Vulkan device.** Check `vulkaninfo --summary`. yserver refuses a
software implementation unless `YSERVER_ALLOW_SOFTWARE_VULKAN` is set,
which is only useful for testing.

**Another user cannot start a display.** Check `ls -ld /tmp/.X11-unix` —
see [The X11 socket directory](#the-x11-socket-directory).

To capture a log for a bug report, note that `starty` redirects the
server's output to its own log file, so piping `starty` itself gets you
only the launcher's messages. Read the server log instead:

```sh
RUST_LOG=debug starty
# after it exits:
cat ~/.local/state/yserver/yserver.log
```

Or run the server directly on a free display:

```sh
RUST_LOG=debug yserver :8 2>&1 | tee /tmp/yserver.log
```

See also `yserver(1)` and `starty(1)`.

## Packaging

`just install` is the packaging entry point. It copies pre-built
artefacts and compiles nothing. Configuration is by environment variable
or `just` assignment — **not** by positional recipe arguments:

```sh
cargo build --locked --release --bin yserver     # %build
PREFIX=/usr just man                             # %build
DESTDIR="$pkgdir" PREFIX=/usr just install       # %install
```

| Variable      | Default                               | Purpose                                |
| ------------- | ------------------------------------- | -------------------------------------- |
| `PREFIX`      | `/usr/local`                          | install prefix                         |
| `DESTDIR`     | empty                                 | staging root                           |
| `TARGETDIR`   | `${CARGO_TARGET_DIR:-target}/release` | where the built binaries are           |
| `TMPFILESDIR` | `$PREFIX/lib/tmpfiles.d`              | set empty to skip the tmpfiles snippet |

Override `TARGETDIR` when building with an explicit `--target`:

```sh
DESTDIR="$pkgdir" PREFIX=/usr \
    TARGETDIR="target/x86_64-unknown-linux-gnu/release" just install
```

Stamp the release commit so `--version` is accurate in a tarball build
with no `.git`:

```sh
YSERVER_GIT_COMMIT=<commit> cargo build --locked --release --bin yserver
```

The packaging `cargo build` above enables no build features on purpose: a
distro package ships a unix-socket-only server with no XDMCP, and
`--version` records that as `features=[]`. Shipping either capability
means adding `--features tcp-transport` or `--features xdmcp`, which is a
per-distro policy decision — see
[Optional build features](#optional-build-features).

Man pages are installed uncompressed and binaries unstripped —
compression, stripping and debuginfo extraction belong to distro tooling.
Everything under `share/doc/` may be relocated or dropped to match distro
policy; only `bin/` and `share/man/man1/` are stable.

Only `yserver` and `starty` are installed. The nested server `ynest` is
not part of the workspace.

Dependencies no automatic scanner can find, because none of them appear
in the ELF headers:

| Need          | Why it is invisible                                           | Package                                                                      |
| ------------- | ------------------------------------------------------------- | ---------------------------------------------------------------------------- |
| Vulkan loader | `libvulkan.so.1` is dlopened at runtime                       | `vulkan-icd-loader`, `vulkan-loader`                                         |
| A Vulkan ICD  | runtime driver, not linked                                    | `mesa-vulkan-drivers` — _recommends_, since the NVIDIA driver also qualifies |
| `xauth`       | `starty` execs it                                             | `xorg-xauth`, `xauth`                                                        |
| `mcookie`     | `starty` execs it if present (else reads `/dev/urandom`)      | `util-linux` — _recommends_                                                  |
| XKB data      | keymap rules read at runtime                                  | `xkeyboard-config`                                                           |
| X core fonts  | _recommends_ only — a fontless system falls back to built-ins | `xorg-fonts-misc`, `xfonts-base`                                             |

Distro packaging recipes live in
<https://github.com/joske/yserver-packaging>.
