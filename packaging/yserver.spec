# RPM spec for yserver — a modern X11 server written from scratch in Rust.
## In honor of Seth Vidal https://www.redhat.com/it/blog/thank-you-seth-vidal

# Builds two binaries:
#   yserver  — standalone DRM/KMS X11 server (drives the GPU directly)
#   ynest    — nested X11 server (runs as a window inside another X server,
#              the Xephyr/Xnest equivalent)
#
# ── Local build ──────────────────────────────────────────────────────────────
#   git archive --format=tar.gz --prefix=yserver-1.0.0/ -o \
#       ~/rpmbuild/SOURCES/yserver-1.0.0.tar.gz HEAD
#   rpmbuild -ba packaging/yserver.spec
#
# ── COPR ─────────────────────────────────────────────────────────────────────
#   Upload this spec; COPR fetches Source0 from GitHub and builds for every
#   enabled chroot/arch. The BuildRequires below are all arch-neutral package
#   names present on every Fedora arch (x86_64, aarch64, …), so no %ifarch
#   guards are needed.
#
#   IMPORTANT: cargo downloads crate dependencies from crates.io during %build.
#   COPR builds run in mock with networking DISABLED by default, so you must
#   tick "Enable internet access during builds" in the COPR project settings
#   (or `copr-cli modify <project> --enable-net on`). Alternatively, ship a
#   `cargo vendor` tree in the tarball and build with `--offline`; see the
#   commented block in %build.

%global crate_name yserver

Name:           yserver
Version:        1.0.0
Release:        1%{?dist}
Summary:        A modern X11 server written from scratch in Rust (DRM/KMS + Vulkan)

# The shipped LICENSE file is MIT. (Note: the workspace Cargo.toml currently
# declares GPL-3.0-only — that mismatch should be reconciled upstream.)
License:        MIT
URL:            https://github.com/joske/yserver
Source0:        %{url}/archive/v%{version}/%{crate_name}-%{version}.tar.gz
#
# For LOCAL testing without pushing, comment the line above and build a
# tarball from your working tree into ~/rpmbuild/SOURCES first (a bare
# filename resolves against %%{_sourcedir}):
#   git archive --format=tar.gz --prefix=%{crate_name}-%{version}/ \
#       -o ~/rpmbuild/SOURCES/%{crate_name}-%{version}.tar.gz HEAD
#Source0:        %{crate_name}-%{version}.tar.gz

Source1:        yserver@.service
Source2:        yserver-session
Source3:        yserver.sysconfig
Source4:        lightdm-99-yserver.conf

# ── Build dependencies ───────────────────────────────────────────────────────
# Rust toolchain. edition = "2024" / resolver = "3" require Rust/Cargo >= 1.85.
BuildRequires:  rust >= 1.85
BuildRequires:  cargo >= 1.85
BuildRequires:  gcc

# build.rs compiles the GLSL composite shaders to SPIR-V with glslc (shaderc).
BuildRequires:  glslc

# C libraries linked by the native crates (libseat, input, xkbcommon,
# freetype-rs, fontconfig, the xshmfence helper, and libudev via the `input`
# stack). pkg-config files come from the matching -devel packages.
BuildRequires:  libseat-devel
BuildRequires:  libxshmfence-devel
BuildRequires:  libxkbcommon-devel
BuildRequires:  libinput-devel
BuildRequires:  fontconfig-devel
BuildRequires:  freetype-devel
BuildRequires:  systemd-devel

# %%systemd_post / %%systemd_preun / %%systemd_postun scriptlets and %%{_unitdir}.
BuildRequires:  systemd-rpm-macros

# ── Runtime dependencies ─────────────────────────────────────────────────────
# The shared libraries linked above are picked up automatically by RPM's ELF
# dependency generator; they are listed here explicitly for documentation.
Requires:       libseat
Requires:       libxshmfence
Requires:       libxkbcommon
Requires:       libinput
Requires:       fontconfig
Requires:       freetype

# yserver renders through Vulkan via `ash`, which dlopen()s libvulkan.so.1 at
# runtime — this is NOT visible to the ELF dependency generator, so require the
# loader explicitly.
Requires:       vulkan-loader

# A Vulkan ICD is needed for actual rendering. Mesa covers virtio-gpu, AMD,
# Intel and (via NVK) Nouveau; proprietary drivers ship their own ICD, so this
# is a Recommends rather than a hard Requires.
Recommends:     mesa-vulkan-drivers

# Core X11 bitmap fonts, for legacy clients that use the server-side font path.
Recommends:     xorg-x11-fonts-misc
Recommends:     xorg-x11-fonts-100dpi
Recommends:     xorg-x11-fonts-75dpi

# Optional: lightdm is the documented way to launch yserver for a graphical
# login (see %%{_datadir}/yserver/lightdm-99-yserver.conf).
Suggests:       lightdm

%description
yserver is a modern X11 server written from scratch in Rust. The goal is not
to clone Xorg but to provide a practical X11 server that runs real desktop
environments, window managers and applications on modern Linux while dropping
legacy baggage (multiple screens, non-TrueColor visuals, indirect GLX, the DDX
driver ABI, endian-swapped clients, and so on).

It drives the GPU directly through atomic DRM/KMS, composites with Vulkan, uses
libseat for seat/VT management and libinput for input. It can run full
MATE/XFCE/Cinnamon desktops and window managers such as FVWM3, e16 and wmaker,
and implements a broad set of extensions (Composite, DAMAGE, DRI3, GLX, Present,
RANDR, RENDER, SHAPE, SYNC, XFIXES, XInput, and more).

This package provides:
  * yserver — the standalone DRM/KMS server
  * ynest   — a nested server that runs inside another X server (like Xephyr)

%prep
%autosetup -n %{crate_name}-%{version}

%build
# Online build (default): cargo fetches dependencies from crates.io.
# Requires network access in the build chroot (see the COPR note in the header).
cargo build --release --locked

# ── Offline / vendored alternative ───────────────────────────────────────────
# If you ship a `cargo vendor` tree in the tarball and a .cargo/config.toml
# that points at it, replace the line above with:
#   cargo build --release --offline
# This makes the build fully hermetic and removes the COPR network requirement.

%install
# Binaries
install -Dm0755 target/release/yserver %{buildroot}%{_bindir}/yserver
install -Dm0755 target/release/ynest   %{buildroot}%{_bindir}/ynest

# systemd template unit + its session launcher and config
install -Dm0644 %{SOURCE1} %{buildroot}%{_unitdir}/yserver@.service
install -Dm0755 %{SOURCE2} %{buildroot}%{_libexecdir}/yserver-session
install -Dm0644 %{SOURCE3} %{buildroot}%{_sysconfdir}/sysconfig/yserver

# Example lightdm drop-in (not active until copied into lightdm.conf.d/)
install -Dm0644 %{SOURCE4} \
    %{buildroot}%{_datadir}/%{name}/lightdm-99-yserver.conf

%post
%systemd_post yserver@.service

%preun
# Template units: %%systemd_preun handles instances via the @ unit name.
%systemd_preun yserver@.service

%postun
%systemd_postun_with_restart yserver@.service

%files
%license LICENSE
%doc README.md docs/high-level-design.md
%{_bindir}/yserver
%{_bindir}/ynest
%{_unitdir}/yserver@.service
%{_libexecdir}/yserver-session
%config(noreplace) %{_sysconfdir}/sysconfig/yserver
%dir %{_datadir}/%{name}
%{_datadir}/%{name}/lightdm-99-yserver.conf

%changelog
* Fri Jun 12 2026 John Boero <boeroboy@gmail.com> - 1.0.0-1
- Initial RPM packaging
- Standalone DRM/KMS server (yserver) and nested server (ynest)
- systemd template unit yserver@.service + session launcher
- Example lightdm drop-in for graphical-login integration
