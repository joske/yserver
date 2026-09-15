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

`yserver` (standalone DRM/KMS) can run full desktop environments.

You can run your steam games on yserver and have it perform 'almost' on par with Xorg. Please try it out and report.

See [`docs/extensions.txt`](docs/extensions.txt) for a list of all supported/implemented extensions.

### Recent work

- XDMCP session negotiation with a display manager, and a TCP listener —
  both optional Cargo build features, off by default; see
  [`docs/setup.md`](docs/setup.md)
- X11 server-side window borders, solid and tiled (awesome)
- damage-clipped repaint on non-composited desktops: only what changed is redrawn,
  cutting compositing GPU load to roughly a third
- direct compositor scanout — a fullscreen compositor pixmap is flipped straight
  to the CRTC instead of being composited first
- DRI3 1.4 syncobjs served through DRM (@ariel3259), with GPU fences published to
  the client's release points
- Present: vsync-off clients uncapped, and no-vsync pacing stabilised (@ariel3259)
- GLX: GetDrawableAttributes/MakeCurrent parity with Xorg, vendor names derived
  from the render driver (@ariel3259)
- per-device pointer acceleration profiles from desktop settings (@ariel3259)
- Mesa OML MSC rate queries (@erpalma)
- reverse-PRIME (@AprilGrimoire)
- a root source Picture now honours IncludeInferiors, so `maim` no longer captures just the background
- no more ghost window on the next workspace after switching (i3 + fastcompmgr)
- a correct cursor with no cursor font installed — the cursor and nil2 fonts are
  bundled instead of falling back to a text font
- FreeBSD VT switching fixed, and Ctrl+C no longer kills the server
- multi-GPU: the render node is resolved on split display/render SoCs, and
  non-desktop connectors are ignored (touchbar on M2 macbook pro)
- binary packages for aarch64 as well as x86_64

### Previously

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

## Hardware tested

- **AMD** — Ryzen 9 6900HX; i9 13900k + RX580 and RX 6800
- **Intel** — i5-7200U iGPU.
- **NVIDIA** — i5 6500 with GTX 1050 (proprietary driver).
- **Snapdragon X1** X1E80100 (Adreno X1, Turnip).
- **Apple** M1 MBA, M2 MBP on Asahi Linux (apple-drm KMS + asahi GPU, Mesa AGX-V).
- **Virtual** — virtio-gpu inside `virtme-ng` (Venus passthrough).

FreeBSD was tested on the i9.

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

- Arch (and derivatives): `yserver` on the AUR
- Fedora, debian, ubuntu and alpine packages in Releases (x86_64 and aarch64)

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
