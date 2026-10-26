starty(1)

# NAME

starty - start yserver on a free display and run a session

# SYNOPSIS

*starty* [_client_ [_client_args_...]]

# DESCRIPTION

*starty* is to *yserver*(1) what *startx*(1) is to Xorg: it brings a
server up on a free display, runs a session against it, and tears the
server down when the session exits.

With no _client_, it runs _~/.xinitrc_, falling back to
_/etc/X11/xinit/xinitrc_. With a _client_, it resolves it via *PATH* and
runs it with the given arguments, no xinitrc needed.

It must be run from a virtual console. *yserver* drives KMS directly and
needs a real controlling console, so *starty* refuses to run from a
pseudo-terminal — over SSH, or in a terminal emulator inside another
graphical session. Switch to a free console first (on Linux,
*Ctrl-Alt-F2*).

The invoking user needs access to _/dev/dri/\*_ and _/dev/input/event\*_;
see *yserver*(1).

*starty* is for console use only. Do not use it as a display manager's X
server command: it forks the server rather than exec'ing it, so the
readiness handshake would never reach the display manager.

# OPERATION

. Scan _/tmp/.X11-unix/_ and pick the lowest free display number.
. Mint an MIT-MAGIC-COOKIE-1 with *mcookie*(1), or from _/dev/urandom_
  where there is no *mcookie*.
. Write the cookie to an unguessable temporary file and pass it to the
  server as *-auth*. This is the server's copy, used only to validate
  connecting clients.
. Add the same cookie to the user's authority file, keyed to the chosen
  display, so clients in the session authenticate — and so a second
  console login can reach the display with a bare *DISPLAY=:N*.
. Start the server and wait up to 30 seconds for its socket.
. Run the session with *DISPLAY*, *XAUTHORITY* and *XDG_SESSION_TYPE=x11*
  set, and *WAYLAND_DISPLAY*/*WAYLAND_SOCKET* unset.
. On exit, terminate the server, remove the display's entry from the
  user's authority file, and delete the temporary server copy.

*XDG_RUNTIME_DIR* is deliberately *not* overridden, so the session
inherits the console login's real _/run/user/UID_ and its
*systemd --user* instance. A _~/.xinitrc_ that repoints
*XDG_RUNTIME_DIR* at a temporary directory will break agent and keyring
sockets.

# OPTIONS

*-h*, *--help*
	Print usage and exit.

# ENVIRONMENT

*RUST_LOG*
	Passed to the server. *starty* defaults it to *warn*, which is quieter
	than *yserver*'s own default of *info*.

*XAUTHORITY*
	Authority file to add the session cookie to. Defaults to
	_~/.Xauthority_.

*XINITRC*
	Alternative xinitrc path, used when no _client_ is given.

# FILES

_$XDG_STATE_HOME/yserver/yserver.log_
	Server output. _XDG_STATE_HOME_ defaults to _~/.local/state_.

_$XDG_STATE_HOME/yserver/session.log_
	Session output.

# REQUIREMENTS

*xauth*(1) must be on *PATH*, along with *yserver*(1). *mcookie*(1) is
used when present.

# PLATFORM NOTES

Works on Linux virtual consoles (_/dev/ttyN_), FreeBSD *vt*(4) consoles
(_/dev/ttyvN_) and OpenBSD *wscons*(4) consoles (_/dev/ttyCN_). Console
takeover — which stops the kernel from turning *Ctrl-C* in an X client
into a signal that kills the whole session — is implemented on all three.

# SEE ALSO

*yserver*(1), *startx*(1), *xauth*(1), *mcookie*(1)
