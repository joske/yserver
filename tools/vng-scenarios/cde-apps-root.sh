# Sourced by tools/vng-shot.sh as guest root once :7 listens (cde-apps).
# dtsession starts ToolTalk's ttsession, which registers with rpcbind and
# resolves the host name; without them CDE puts up a system-modal "Action
# Required" dialog that holds the focus.
# shellcheck shell=sh
if command -v rpcbind > /dev/null; then
    install -d /run/rpcbind
    rpcbind > rpcbind.log 2>&1 || echo "rpcbind failed" >> rpcbind.log
fi
grep -q "[[:space:]]$(hostname)\$" /etc/hosts 2> /dev/null || echo "127.0.0.1 $(hostname)" >> /etc/hosts
