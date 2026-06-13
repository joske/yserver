# yserver

A modern X11 server written from scratch in Rust.

The goal is not to clone Xorg. It is to provide a practical X11 server that
runs real desktop environments, window managers, and applications on modern
Linux while dropping legacy baggage (multiple screens, non-TrueColor visuals,
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
- XInputExtension
- XKEYBOARD
- XTEST

### GLX_EXT_texture_from_pixmap

Implemented and tested on AMD, intel, Asahi and Qualcomm. It can NOT (read: NEVER) work on nvidia proprietary driver, and on
the only nvidia card I have (GTX 1050), the nouveau driver can not even bring up Xorg. Nouveau may work on other
cards, but untested.

## Demo

With TFP implemented, we now support compiz, demo here:



https://github.com/user-attachments/assets/dc266c55-e9ee-4649-a0c4-be3db2526713



## Hardware tested

`yserver` (standalone DRM/KMS) has been tested end-to-end against a
MATE / xfce4 / Cinnamon desktop on:

- **AMD** — Ryzen 9 6900HX (Rembrandt, RDNA2, RADV); i9 13900k + RX580
  (Polaris/GCN4, RADV).
- **Intel** — i5-7200U (Kaby Lake, ANV) iGPU.
- **NVIDIA** — i5 6500 with GTX 1050 (proprietary driver).
- **Snapdragon X1** X1E80100 (Adreno X1, Turnip). 
- **Apple** M1 MBA, M2 MBP on Asahi Linux (apple-drm KMS + asahi GPU, Mesa AGX-V).
- **Virtual** — virtio-gpu inside `virtme-ng` (Venus passthrough).

## Running the standalone DRM/KMS server

`yserver` uses libseat for seat management if available.
It can also drive atomic KMS directly, but then your user needs access to /dev/dri/ and to /dev/input/.

It requires a recent stable Rust toolchain and the following dependencies:

#### Arch

```sh
sudo pacman -S just gcc seatd libxshmfence libxkbcommon libinput shaderc systemd-libs fontconfig
```

#### Ubuntu

```sh
sudo apt install just gcc libseat-dev libxshmfence-dev libxkbcommon-dev libinput-dev glslc libudev-dev libfontconfig-dev
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

Known limitation: **VT switching does not work** in a lightdm-launched session
yet — yserver runs rootful without a logind session there, so it can't use
libseat for VT control ([#10](https://github.com/joske/yserver/issues/10)).

## Use directly on TTY

```sh
## switch to a free TTY, then run:
just startx
```

which will start yserver and then execute your `~/.xinitrc` (or fall back to `/etc/X11/xinit/xinitrc`)

    If you are using libseat, you can switch VT, but if you use direct, you CAN NOT switch VT when yserver is running. Zap the server, or log out of your session otherwise.


Some convenience keybinds are available:

- Ctrl-Alt-Backspace: zap the server, return to console
- Ctrl-Alt-Enter: create a screenshot/scanout of the framebuffer in CWD
- Ctrl-Alt-F12: dump all drawables as PPM files to CWD

## Regression coverage with xts5 and rendercheck

We run the X.Org X Test Suite (xts5) against `yserver` to gauge protocol completeness.

Latest pass numbers per scenario live in [`docs/test-status.md`](docs/test-status.md).

## License

This project is licensed under the MIT license. Please check [LICENSE](LICENSE).
