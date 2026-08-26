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
DRM/RANDR timing, monitor identity, programmable clock capability, viewport
and gamma ramp. This
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

- `starty` launcher (startx-style: display number picking, MIT-MAGIC-COOKIE-1, session teardown)
- Present completions paced to the vblank, and the Present 1.4 acquire timeline honored
  (fixes free-running clients, fullscreen video, Plasma's spinner)
- keyboard layout switching: stale per-key overrides cleared and a legacy
  MappingNotify broadcast, so clients pick up the new layout (@azytar)
- `FreeColors` dispatched instead of failing with `BadRequest` (Tk compatibility)
  (@erpalma)
- GLX pbuffers, backed by a real GPU pixmap — Chrome and Steam now get GPU acceleration
- scanout buffers allocated via GBM and imported into Vulkan (wider driver/modifier coverage)
- yserver is now truly idle when nothing happens
- core-loop fairness and backpressure
- RANDR output properties, read _and_ write (minors 12/13/14)
- RANDR output identity passthrough: EDID, EDID_DATA, ConnectorType
- RANDR fractional refresh rates from real kernel mode timings
- Xorg-compatible output names (HDMI-1, not HDMI-A-1)
- X-Resource client accounting with LocalClientPID in QueryClientIds
- per-device libinput settings (mice and touchpads) actually apply
- XI1 feedback controls, resolution control, real event selections
- pointer motion history (GetMotionEvents)
- XI2: relative RawMotion deltas, scroll-stop on finger-lift, implicit pointer grabs,
  unified freeze state
- XTEST cursor comparison
- RENDER: full standard PictOp family, a8-mask destinations for CompositeGlyphs,
  ClipByChildren parity for Composite/Glyphs/Traps/Tris
- perf: glyph rendering batched into one instanced draw per text run
- cursor recoloring, and depth-1 wire bitmaps for XCreatePixmapCursor
- XKB group state and GetMap component filtering (@AprilGrimoire)
- CopyArea source resolution simplified (@AprilGrimoire)
- stable-toolchain build fixed on Ubuntu 26.04 (@erpalma)
- Direct-only seat model (libseat dependency dropped)
- much better responsiveness on NVIDIA GPUs
- newly working apps: Steam (WebGL), ImageMagick `import` region-select,
  KDE Plasma RandR resize + ARGB popups (@erpalma), fullscreen games/video under Cinnamon
- newly tested and fixed desktops: plasma X11 (sonic DE)
- binary releases for Fedora/Debian/Ubuntu/Alpine

### Previously

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
- leftwm

## Hardware tested

- **AMD** — Ryzen 9 6900HX (Rembrandt, RDNA2, RADV); i9 13900k + RX580 (Polaris/GCN4, RADV); AMD Ryzen AI 9 HX 370 w/ Radeon 890M.
- **Intel** — i5-7200U (Kaby Lake, ANV) iGPU.
- **NVIDIA** — i5 6500 with GTX 1050 (proprietary driver).
- **Snapdragon X1** X1E80100 (Adreno X1, Turnip).
- **Apple** M1 MBA, M2 MBP on Asahi Linux (apple-drm KMS + asahi GPU, Mesa AGX-V).
- **Virtual** — virtio-gpu inside `virtme-ng` (Venus passthrough).

FreeBSD was tested on the i9 (GhostBSD).

## Installation and setup

Full instructions — dependencies per distro, device access, display
manager and console startup, key bindings, troubleshooting and packaging
— are in **[docs/setup.md](docs/setup.md)**, also installed to
`$prefix/share/doc/yserver/setup.md`.

The short version, from a source checkout:

```sh
just install-local                   # builds and installs to /usr/local
sudo usermod -aG video,input $USER   # then log out and back in
```

Then switch to a free console and run `starty`. yserver drives atomic KMS
directly with no seat manager, so it needs access to `/dev/dri/*` and
`/dev/input/event*` and a working Vulkan driver.

### Packages

- Arch (and derivatives): `yserver` and `yserver-git` on the AUR
- Fedora, debian, ubuntu and alpine packages in Releases

The release files are loose packages, not repositories, so install them by path:

```sh
sudo dnf install ./yserver-*.rpm
sudo apt install ./yserver_*.deb
sudo apk add --allow-untrusted ./yserver-*.apk   # signed with a per-build key
```

See also `yserver(1)` and `starty(1)`.

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
