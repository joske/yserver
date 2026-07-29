# yserver

A modern X11 server written from scratch in Rust.

The goal is not to clone Xorg. It is to provide a practical X11 server that
runs real desktop environments, window managers, and applications on modern
Linux while dropping legacy baggage (non-TrueColor visuals,
indirect GLX, the DDX driver ABI, endian-swapped clients, and so on).

See [`docs/high-level-design.md`](docs/high-level-design.md) for the full design and scope.

## Name

The `yserver` name is the 'working' name as it was the first idea that popped into my head when
starting the project. But there are multiple projects on GitHub with this name (but none for X11 servers),
the name is subject to change. Not a priority now.

## Status

`yserver` (standalone DRM/KMS) can now run full MATE/XFCE/Cinnamon desktops.
Other tested window managers include FVWM3, e16 and wmaker.

We support the following extensions:
- BIG-REQUESTS
- Composite
- DAMAGE
- DPMS
- DRI3
- GLX
- Generic Event Extension
- MIT-SCREEN-SAVER
- MIT-SHM
- Present
- RANDR
- RENDER
- SHAPE
- SYNC
- X-Resource
- XFIXES
- XINERAMA
- XInputExtension
- XC-MISC
- XFree86-VidModeExtension
- XKEYBOARD
- XTEST

### GLX_OML_sync_control

Mesa implements `glXGetMscRateOML()` on the client side by reading the current
mode through `XFree86-VidModeExtension`. Yserver implements Xorg's read surface
with the correct legacy/v2 wire layouts and the selected output's real
DRM/RANDR timing, monitor identity, dot clock, viewport and gamma ramp. This
lets Mesa and ANGLE-based Flatpak clients derive the display MSC rate while
other VidMode readers see data consistent with RANDR instead of unexpected
`BadRequest` errors.

VidMode remains deliberately read-only because RANDR owns display
configuration. `GetPermissions` advertises only `XF86VM_READ_PERMISSION`, and
known mode/gamma writes fail with VidMode's `ClientNotLocal` error — the same
coherent fallback branch Xorg exposes to a client without write permission.
Yserver clients are physically local Unix-socket peers, so this is an explicit
server policy rather than a claim that they are remote.

### GLX_EXT_texture_from_pixmap

Implemented and tested on AMD, intel, Asahi and Qualcomm. It can NOT (read: NEVER) work on nvidia proprietary driver, and on
the only nvidia card I have (GTX 1050), the nouveau driver can not even bring up Xorg. Nouveau may work on other
cards, but untested.

### Recent work
- FreeBSD now works
- Display hotplug now works
- xauth support (no xhost ACL support - not needed as we're unix socket only)
- XINERAMA support
- RANDR gamma correction (redshift)
- direct mode VT switch
- XKB runtime layout change
- LEDs driven from XKB state
- XFIXES pointer barriers
- XC-MISC XID recycling
- musl (Alpine) build working
- newly tested and fixed desktops: bspwm/sxhkd, enlightenment 0.27, icewm, awesome, openbox, picom, blackbox
- many many bugfixes

## Demo

With TFP implemented, we now support compiz, demo here:



https://github.com/user-attachments/assets/dc266c55-e9ee-4649-a0c4-be3db2526713


## Tested WMs/desktops
`yserver` has been tested end-to-end against the following WMs/desktops: 
- Cinnamon
- MATE
- XFCE
- FVWM3
- sonic (KDE plasma X11 fork)
- wmaker
- openbox
- awesome
- picom
- compiz
- icewm
- blackbox
- bspwm/sxhkd
- i3/fastcompmgr
- enlightenment e16 + e27

## Hardware tested

- **AMD** — Ryzen 9 6900HX (Rembrandt, RDNA2, RADV); i9 13900k + RX580
  (Polaris/GCN4, RADV).
- **Intel** — i5-7200U (Kaby Lake, ANV) iGPU.
- **NVIDIA** — i5 6500 with GTX 1050 (proprietary driver).
- **Snapdragon X1** X1E80100 (Adreno X1, Turnip). 
- **Apple** M1 MBA, M2 MBP on Asahi Linux (apple-drm KMS + asahi GPU, Mesa AGX-V).
- **Virtual** — virtio-gpu inside `virtme-ng` (Venus passthrough).

FreeBSD was tested on the i9 (GhostBSD).

## Running the standalone DRM/KMS server

> [!IMPORTANT]
> `yserver` drives atomic KMS directly, your user needs access to /dev/dri/ and to /dev/input/.

On most systems, you can do `sudo usermod -aG video,input $USER` then re-login. 

It requires a recent stable Rust toolchain and the following dependencies:

#### Arch

```sh
sudo pacman -S --needed just gcc libxshmfence libxkbcommon libinput shaderc systemd-libs fontconfig pkgconf mesa
```

#### Ubuntu

```sh
sudo apt install just gcc libxshmfence-dev libxkbcommon-dev libinput-dev glslc libudev-dev libfontconfig-dev libgbm-dev
```

#### Alpine

```sh
export RUSTFLAGS="-C target-feature=-crt-static"
apk add gcc musl-dev fontconfig-dev freetype-dev libxshmfence-dev libxkbcommon-dev libinput-dev shaderc mesa-dev
```

#### FreeBSD


```sh
doas pkg install -y shaderc fontconfig libudev-devd GhostBSD-bzip2-dev GhostBSD-zlib-dev
```

## Use with a display manager (lightdm)

`lightdm` can launch yserver as its X server for a graphical login (its
        X-server command is configurable, unlike gdm/sddm). 

1. Install the binary (requires sudo): `just install` (installs it at `/usr/local/bin/yserver`).
2. Point lightdm at it — create `/etc/lightdm/lightdm.conf.d/99-yserver.conf`:

```ini
[Seat:*]
xserver-command=/usr/local/bin/yserver
```

3. From a free TTY, restart lightdm: `sudo systemctl restart lightdm`.

The greeter appears, you log in, and the login keyring is unlocked by lightdm's PAM stack.

## Use directly on TTY

The easiest way is the `starty` launcher (install it with `just install`, which
puts both `yserver` and `starty` in `/usr/local/bin`):

```sh
## switch to a free TTY, then run:
starty                 # runs ~/.xinitrc (or /etc/X11/xinit/xinitrc)
starty bspwm           # ...or a WM resolved via PATH, no xinitrc needed
```

`starty` mirrors real `startx`: it picks the lowest free display, mints a
per-session MIT-MAGIC-COOKIE-1 (server copy + entry in `~/.Xauthority`), waits
for the socket, runs the session, and tears the server down on exit. Server
and session logs land in `$XDG_STATE_HOME/yserver/` (`~/.local/state/yserver/`).
`video`/`input` group access (above) is still required — yserver opens
`/dev/dri` and `/dev/input` directly, with no seat manager.

From a source checkout you can also use `just startx`, which does the same thing
against the in-tree debug build.

Some convenience keybinds are available:

- Ctrl-Alt-Backspace: zap the server, return to console
- Ctrl-Alt-Enter: create a screenshot/scanout of the framebuffer in CWD
- Ctrl-Alt-F12: dump all drawables as PPM files to CWD

## Regression coverage with xts5 and rendercheck

We run the X.Org X Test Suite (xts5) against `yserver` to gauge protocol completeness.

Latest pass numbers per scenario live in [`docs/test-status.md`](docs/test-status.md).

To run XTS yourself, you need to install the following extra packages:
```bash 
sudo pacman -S --needed autogen automake autoconf make xtrans xterm xorg-xset xorg-fonts-misc xorg-xdpyinfo xorg-bdftopcf xorg-mkfontscale xorg-util-macros
```
Then clone `https://gitlab.freedesktop.org/xorg/test/xts.git` next to the yserver repo. 
Build it with:
```bash
./autogen.sh
make
```

Switch to a free VT
and use `just xts-yserver-hw`. It takes about 50 minutes, don't touch mouse/kb, xts drives the mouse on some tests.



## Contributing

Contributions are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md) for the toolchain, the checks CI runs, and the signed-commit requirement.

## License

This project is licensed under the MIT license. Please check [LICENSE](LICENSE).
