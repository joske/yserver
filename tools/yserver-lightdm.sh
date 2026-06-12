#!/usr/bin/env bash
# lightdm launches this as its X server (xserver-command). lightdm appends
# X-style argv (:N -seat seatN -auth FILE -nolisten tcp vtN -novtswitch),
# which yserver parses natively (see crates/yserver/src/launch.rs). This
# wrapper only adds logging + backtraces; it execs yserver so the SIGUSR1
# readiness handshake reaches lightdm (the real parent), not a shell.
exec env RUST_LOG="${YSERVER_LOG:-info}" RUST_BACKTRACE=1 \
    /usr/local/bin/yserver "$@" 2>>/var/log/yserver-lightdm.log
