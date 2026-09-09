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
- Linux with atomic KMS/DRM, or FreeBSD.
- Access to `/dev/dri/*` and `/dev/input/event*` — see
  [Device access](#device-access).

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

```sh
doas pkg install -y shaderc fontconfig libudev-devd scdoc GhostBSD-bzip2-dev GhostBSD-zlib-dev
```

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

To run a session you also need `xauth` and `mcookie` at runtime —
`xorg-xauth` and `util-linux` respectively.

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
```

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

TCP is off by default, as on a modern Xorg build. To enable it:

```sh
yserver :7 -auth /path/to/authority -listen tcp
```

The port is 6000 plus the display number, and the server binds `0.0.0.0` —
every interface, not only loopback. `-listen tcp` refuses to start without a
`-auth` file containing a usable MIT-MAGIC-COOKIE-1, because the alternative
would be an unauthenticated X server on the network.

**There is no host-based access control.** `xhost` is not implemented, so the
cookie is the entire boundary and there is no way to restrict which hosts may
connect. The wire protocol is unencrypted, keystrokes included. Read the
SECURITY section of `yserver(1)` before exposing a display this way.

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
| `mcookie`     | `starty` execs it                                             | `util-linux`                                                                 |
| XKB data      | keymap rules read at runtime                                  | `xkeyboard-config`                                                           |
| X core fonts  | _recommends_ only — a fontless system falls back to built-ins | `xorg-fonts-misc`, `xfonts-base`                                             |

Distro packaging recipes live in
<https://github.com/joske/yserver-packaging>.
