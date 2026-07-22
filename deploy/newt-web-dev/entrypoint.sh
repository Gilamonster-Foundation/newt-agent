#!/bin/sh
# newt-web-dev entrypoint — the security-sensitive init, as a lintable file
# (review finding: never bury this in a CMD shell string).
#
# Contract: the drake-interactive-hostkeys Secret mounts at /run/hostkeys and
# MUST contain exactly ssh_host_ed25519_key and ssh_host_rsa_key (pubs
# optional). Keys are copied ONCE at startup: rotating the Secret does NOT
# rotate the live identity — roll the pod (see README).
set -eu

src=/run/hostkeys
dst=/etc/ssh

for key in ssh_host_ed25519_key ssh_host_rsa_key; do
    test -s "$src/$key" || { echo "entrypoint: missing $src/$key" >&2; exit 1; }
    install -m 0600 -o root -g root "$src/$key" "$dst/$key"
    if test -f "$src/$key.pub"; then
        install -m 0644 -o root -g root "$src/$key.pub" "$dst/$key.pub"
    fi
done

# Validate the sshd config + keys BEFORE replacing PID 1.
/usr/sbin/sshd -t -h "$dst/ssh_host_ed25519_key" -h "$dst/ssh_host_rsa_key"

# The cockpit runs unprivileged beside sshd (dev-box supervision: if it dies
# the box lives; restart over SSH or re-roll). Bind per NEWT_WEB_BIND (D3).
su drake -c "RUST_LOG=info NEWT_WEB_BIND='${NEWT_WEB_BIND:-127.0.0.1:8880}' /usr/local/bin/newt-web" &

exec /usr/sbin/sshd -D -e -h "$dst/ssh_host_ed25519_key" -h "$dst/ssh_host_rsa_key"
