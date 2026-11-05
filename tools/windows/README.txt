yserver for Windows
===================

yserver is an X11 server. This build shows X programs that run elsewhere,
in WSL or on another machine, on the Windows desktop. Clients connect over
TCP, to port 6000 + the display number.

https://github.com/joske/yserver


Starting it
-----------

Run start-yserver.cmd. It starts yserver on display :0, and on the first
run it creates a cookie file in %LOCALAPPDATA%\yserver\Xauthority (set
YSERVER_AUTH to use another file). It prints the cookie, which clients
need to connect. yserver's icon in the notification area opens its
Settings and quits it (Exit).

Windows Firewall asks whether yserver may accept connections the first time
it runs. Allow it on the networks your clients come from; if WSL2 cannot
connect, allow it on public networks too.

The mode is chosen in Settings and applies when yserver restarts (its
Restart button does that); --rootless or --rooted overrides it for one run.

Rootless (the default): each X window is a window of its own on the
Windows desktop.

Rooted: one window holds the whole X screen, for a desktop session or a
window manager; its title bar menu also opens Settings. --geometry WxH
sets its size.

Settings are kept in %APPDATA%\yserver\settings.conf.

The keyboard layout is US unless you pass one, e.g. `-layout de` (any
other argument to start-yserver.cmd is passed on too), or set
XKB_DEFAULT_LAYOUT.


Connecting from WSL
-------------------

WSLg sets DISPLAY to its own X server, so set it to yserver's:

  # WSL2 with the default NAT networking: Windows is the default gateway
  export DISPLAY=$(ip route show default | awk '{print $3}'):0
  # WSL2 with mirrored networking, or WSL1
  export DISPLAY=127.0.0.1:0

and use the cookie file directly (it matches any host):

  export XAUTHORITY=/mnt/c/Users/<you>/AppData/Local/yserver/Xauthority
  xterm &


Connecting from another machine
-------------------------------

Add the cookie start-yserver.cmd printed to your Xauthority, then point
DISPLAY at the Windows machine:

  xauth add <windows-host>:0 MIT-MAGIC-COOKIE-1 <cookie>
  export DISPLAY=<windows-host>:0

X11 is not encrypted. Over an untrusted network, use ssh's X11 forwarding
instead (below).


ssh -Y from Windows
-------------------

Windows' own ssh needs xauth to hand the cookie to the remote X clients;
this zip has one. Tell ssh where it is, in %USERPROFILE%\.ssh\config:

  XAuthLocation "C:\path\to\yserver\xauth.exe"

then, in cmd:

  set DISPLAY=127.0.0.1:0
  ssh -Y remote-host

Not :0 (yserver has no local socket), and not localhost:0, which works but
makes every X connection 2 seconds slower. xauth.exe reads XAUTHORITY, else
YSERVER_AUTH, else the cookie file start-yserver.cmd made.

Use -Y, not -X: -X asks for an untrusted cookie, which needs the X SECURITY
extension yserver does not have, so ssh says "untrusted X11 forwarding setup
failed" and forwards nothing. `ForwardX11Trusted yes` in the config makes -X
the same as -Y.


Graphics
--------

yserver draws with Vulkan and needs a GPU whose driver supports Vulkan 1.3
(or 1.2 with VK_KHR_dynamic_rendering and VK_KHR_synchronization2). Without
one (no GPU driver, the Microsoft Basic Display Adapter, an old GPU, most
VMs) it stops and says why.

The fallback is lavapipe, Mesa's software Vulkan driver, from mesa-dist-win
(https://github.com/pal1000/mesa-dist-win). Unpack it, and before starting
yserver set VK_DRIVER_FILES to its lavapipe ICD file, e.g.

  set VK_DRIVER_FILES=C:\mesa\x64\lvp_icd.x86_64.json
  start-yserver.cmd

It is slower, and uses the CPU for drawing.


Files
-----

  yserver.exe          the server
  start-yserver.cmd    starts it with a cookie
  xauth.exe            the cookie for ssh -Y (`xauth list` only)
  xkb\                 keyboard data (xkeyboard-config); XKB_CONFIG_ROOT
                       overrides it
  LICENSE.txt          yserver's license
  licenses\            the licenses of the libraries and data in this build

yserver.exe needs no Visual C++ Redistributable: the C runtime is linked
in.

For more output, set RUST_LOG=debug before starting it.
