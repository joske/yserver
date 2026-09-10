yserver(1)

# NAME

yserver - X11 server for Linux KMS/DRM

# SYNOPSIS

*yserver* [:_display_] [vt_N_] [-auth _file_] [-displayfd _fd_] [-layout _layout_] [--version]

# DESCRIPTION

*yserver* is an X11 server written from scratch in Rust. It is not an Xorg
derivative. It drives atomic KMS/DRM directly and renders with Vulkan.

There is no seat manager. The server process opens _/dev/dri/\*_ and
_/dev/input/event\*_ itself, so it needs read/write access to both, and it
requires a working Vulkan driver. How that access is obtained depends on
how the server is started:

*Launched by a display manager*
	The server runs as root and already has access. See
	_@DOCDIR@/examples/lightdm-99-yserver.conf_.

*Launched by the user from a console*
	Use *starty*(1). The invoking user needs the _video_ and _input_
	groups, or equivalent seat ACLs:

	```
	sudo usermod -aG video,input $USER
	```

	then log out and back in.

# OPTIONS

*:*_display_
	Display number to serve, e.g. *:1*. A bare number is also accepted.
	Defaults to *:7*.

*vt*_N_
	Use virtual console _N_, e.g. *vt7*.

*-auth* _file_
	Read the authority file used to validate connecting clients. This is
	the server's own copy; it is not the client-side _XAUTHORITY_.

*-displayfd* _fd_
	Write the chosen display number to _fd_ once the socket is listening,
	instead of taking the display from the command line.

*-layout* _layout_
	XKB layout name, e.g. *be*.

*-listen* _protocol_, *-nolisten* _protocol_
	Enable or disable a transport. Only *tcp* is understood; any other
	value is accepted and ignored so a display manager's argv does not
	fail. The options are ordered and the last one for a transport wins,
	so *-listen tcp -nolisten tcp* leaves TCP disabled.

	TCP is *disabled by default*, as on a modern *Xorg*(1) build. With
	*-listen tcp* the server binds *0.0.0.0* on port 6000 plus the display
	number — every interface, not just loopback.

	*-listen tcp* requires *-auth* with a usable MIT-MAGIC-COOKIE-1 file
	and refuses to start without one. Read the warning under *SECURITY*
	before using it.

*-noreset*, *-reset*, *-terminate*
	What to do when the last client that completed connection setup
	disconnects. *-noreset* is the default and does nothing, so the server
	outlives its clients — this is what *starty*(1) and an ordinary
	desktop session want. *-reset* destroys the session and returns the
	server to a freshly-started state, ready for the next one. *-terminate*
	exits instead. Ordered and last-wins.

	Note the default is the opposite of *Xorg*(1), which resets. A stray
	reset in a desktop session is indistinguishable from a crash, and
	nothing that launches *yserver* today expects one.

	Under *-reset*, *any* client that is the last one to disconnect
	triggers a reset — including a one-shot tool. Running *xprop*(1) or
	*xdpyinfo*(1) against an otherwise idle *-reset* server therefore
	destroys the session as that tool exits. This matches *Xorg*(1), and it
	means *-reset* only makes sense when something holds the display open
	continuously, such as a display manager's session.

*-query* _host_, *-indirect* _host_, *-broadcast*
	Run as an *XDMCP* display: ask a display manager for a session
	instead of starting one locally. *-query* asks one manager directly,
	*-indirect* asks it to choose, *-broadcast* asks whoever answers.
	Ordered and last-wins, so *-query a -broadcast* ends up broadcasting.

	These imply *-reset*, and *-once* then implies *-terminate*: one
	XDMCP session is one server generation, so logging out wipes the
	session and queries again for the next user.

	They also require *-listen tcp* in practice, because the session's
	clients run on the manager's host. No *-auth* is needed — the
	manager supplies a per-session cookie in its reply, and that cookie
	is the *only* thing that authorizes a TCP client while XDMCP is
	driving. See *SECURITY*.

	*-broadcast* sends to the limited broadcast address rather than
	enumerating each interface's broadcast address as *Xorg*(1) does.
	That is correct for a single-homed machine, which is the case this
	was built for; on a multi-homed host it will not reach the same set
	of managers.

*-port* _n_
	UDP port of the display manager. Default *177*.

*-from* _address_
	Source address to send XDMCP packets from.

*-class* _string_, *-displayID* _string_
	Display class and identifier reported to the manager. The class
	defaults to *MIT-unspecified*.

*-once*
	Exit rather than renewing. This covers every renewal condition, not
	only a clean logout: a keepalive failure, or exhausting the
	retransmission budget against a manager that never answers, also
	exits.

*--version*, *-version*
	Print the version and the commit it was built from, then exit.

## ACCEPTED FOR COMPATIBILITY, IGNORED

Display managers and *xinit*(1) pass these unconditionally. *yserver*
parses them so startup does not fail, but they have no effect:

*-seat* _seat_
	Parsed and discarded. Input is always assigned to *seat0*.

*-novtswitch*
	No-op.

*-config* _file_, *-background* _spec_
	Parsed with their value and discarded.

# KEY BINDINGS

*Ctrl-Alt-Backspace*
	Terminate the server immediately and return to the console.

*Ctrl-Alt-F1* .. *Ctrl-Alt-F11*
	Switch to virtual console 1 to 11.

*Ctrl-Alt-Enter*
	Write the current scanout to a PPM file in the working directory.

*Ctrl-Alt-F12*
	Write every drawable's storage to PPM files in the working directory.

# SIGNALS

*SIGUSR1* (outgoing, to the parent process)
	Sent once when the server is ready to accept clients, but only if
	*SIGUSR1* was inherited with disposition *SIG_IGN* — the convention a
	display manager uses to request the readiness handshake. *lightdm*
	relies on this.

*SIGUSR1* (incoming)
	Release the virtual console. Part of the VT handover handshake, and
	unrelated to the readiness signal above.

*SIGUSR2* (incoming)
	Acquire the virtual console.

*SIGTERM*, *SIGINT*
	Shut down, restoring the console.

*SIGHUP*
	Depends on the reset policy, and is deliberately not uniform:

[[ *policy*
:[ *SIGHUP does*
|  *-noreset* (default)
:  shuts down, restoring the console
|  *-reset*
:  forces a reset
|  *-terminate*
:  forces a reset, *not* a terminate

	Forcing a reset under *-terminate* follows *Xorg*(1), where a hangup
	always resets and *-terminate* governs only what the last client
	leaving does. The *-noreset* row is the exception: a hangup has always
	stopped *yserver*, and making it destroy a default session instead
	would be a surprising change to a signal people already send.

The diagnostic dumps are *not* signal-driven; they are the key bindings
above.

# ENVIRONMENT

*RUST_LOG*
	Log filter. Defaults to *info*; *warn* is quieter, *debug* and *trace*
	are for diagnosis.

*YSERVER_DRM_DEVICE*
	Path to the sole KMS device to use instead of auto-selecting devices,
	e.g. _/dev/dri/card1_. Ignored when *YSERVER_DRM_DEVICES* is set.

*YSERVER_DRM_DEVICES*
	Comma-separated ordered KMS-device paths, e.g.
	_/dev/dri/card1,/dev/dri/card0_. This overrides
	*YSERVER_DRM_DEVICE*. The first entry that opens successfully supplies
	the startup scanout device; remaining entries are retained as secondary
	KMS providers. Commas delimit entries so stable by-path names may retain
	their PCI-address colons.

*YSERVER_DRI_RENDER_NODE*
	Path to the Vulkan/DRI3 render node to use instead of resolving one from
	the first successfully opened KMS device, e.g. _/dev/dri/renderD128_.
	Useful on split display/render systems and systems without sysfs-style
	card/render parent metadata.

*YSERVER_MODE*
	Force a mode instead of the connector's preferred mode, as
	_WIDTHxHEIGHT_ — for example *1920x1080*. A refresh-rate suffix is not
	accepted.

*YSERVER_ALLOW_SOFTWARE_VULKAN*
	If set to any value, permits a software Vulkan implementation such as
	lavapipe. Refused by default, being far too slow for interactive use.

*YSERVER_GLX_VENDOR*
	Override the *GLX_VENDOR_NAMES_EXT* reply instead of deriving it from
	the render driver, as a libglvnd vendor name or space-separated
	fallback list, e.g. *nvidia* or *nvidia mesa*. An empty or
	whitespace-only value is ignored and falls back to the derived value.
	Read once by the server at startup, so changing it requires
	restarting the display server — unlike libglvnd's own
	*\_\_GLX_VENDOR_LIBRARY_NAME*, which is read by the client and takes
	effect on the next client launch.

Further *YSERVER_*\* variables exist for development tracing and are
deliberately not documented here; see the project documentation.

# SECURITY

With the default options *yserver* listens on a UNIX socket only, and the
socket's filesystem permissions are the access control.

*-listen tcp* changes that completely. The server binds *0.0.0.0* — every
interface — and the *only* thing standing between the network and the display
is the MIT-MAGIC-COOKIE-1 cookie. There is no host-based access control:
*xhost*(1) is not implemented yet, so there is nothing to restrict which hosts
may connect, and no way to grant or revoke access per host.

Consequences worth being explicit about:

- Anyone who can reach the port and obtain the cookie has the display: all
  input, all window contents, and the ability to inject events.
- The cookie is the whole boundary, so treat the authority file as a secret.
  *-listen tcp* refuses to start without one precisely because the failure
  mode of forgetting it would be an open X server on the network.
- The X11 wire protocol is *not encrypted*. Everything, including keystrokes,
  crosses the network in clear text.

For ordinary remote use, prefer *ssh -X*, which needs none of this: it proxies
through a local UNIX socket on the remote host and never touches the TCP
listener. *-listen tcp* exists for XDMCP, where the display manager supplies a
per-session cookie and the network is assumed to be trusted.

# XDMCP AND TRUST

XDMCP as implemented here has *no authentication of the manager*.
XDM-AUTHENTICATION-1, the protocol's optional mutual authentication, is not
implemented — so anything on the network that can answer your query can offer
you a session, and everything including keystrokes crosses the network in
clear text. This is equally true of *Xorg*(1) in the same configuration, and
is why XDMCP deployments assume a trusted, controlled network.

Two hardening choices deliberately differ from *Xorg*(1), both because its
behaviour looks like an oversight:

- An *Unwilling* reply is acted on only while actually collecting replies, and
  under *-query* only from the manager that was asked. In *Xorg* any host may
  end a *running* session with a single packet.
- The retransmission budget is reset only by a packet that is valid for the
  current state. In *Xorg* any datagram resets it, so a peer sending rubbish
  can keep a display retrying forever and stop *-once* from ever exiting.

# FILES

_/tmp/.X11-unix/X<display>_
	The listening socket.

_@DOCDIR@/setup.md_
	Setup guide: dependencies, device access, display manager and console
	startup, troubleshooting.

_@DOCDIR@/examples/lightdm-99-yserver.conf_
	Example lightdm drop-in.

# SEE ALSO

*starty*(1), *Xserver*(1), *xauth*(1), *lightdm*(1)

# BUGS

Report at https://github.com/joske/yserver/issues
