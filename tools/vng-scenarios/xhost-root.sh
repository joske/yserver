# Sourced by tools/vng-shot.sh as guest root once :7 listens (xhost).
# Adds a second throwaway user, ysuite2, and runs each line xhost.sh writes
# to /run/xhost/req as ysuite2 without a cookie, answering its exit status
# on /run/xhost/rep; the line `exit` stops it.
# shellcheck shell=sh
# shellcheck disable=SC2154 # uid, gid: ysuite's, set by guest.sh
u2=4343
sed -i "/^[^:]*:[^:]*:$u2:/d" /etc/passwd; sed -i "/^[^:]*:[^:]*:$u2:/d" /etc/group
echo "ysuite2:x:$u2:$u2:vng suite 2:/run/ysuite2:/bin/sh" >> /etc/passwd
echo "ysuite2:x:$u2:" >> /etc/group
install -d -m 0700 -o $u2 -g $u2 /run/ysuite2
install -d -m 1777 /run/xhost
mkfifo -m 0600 /run/xhost/req /run/xhost/rep
chown "$uid:$gid" /run/xhost/req /run/xhost/rep
(
    set +e
    while read -r cmd < /run/xhost/req; do
        [ "$cmd" != exit ] || break
        setpriv --reuid=$u2 --regid=$u2 --clear-groups env -i PATH="$PATH" HOME=/run/ysuite2 \
            DISPLAY=:7 XAUTHORITY=/dev/null sh -c "$cmd" < /dev/null >> xhost-u2.log 2>&1
        echo $? > /run/xhost/rep
    done
) > /dev/null 2>&1 &
