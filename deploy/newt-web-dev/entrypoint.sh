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

# The cockpit runs unprivileged beside sshd, and SELF-HEALS: a supervisor
# subshell respawns newt-web whenever it exits, so a crash never leaves the
# pod green-but-web-dead (the failure mode behind the transient 500s during
# live testing). sshd stays PID 1, so live SSH sessions survive a web restart,
# and the /healthz readiness probe pulls the pod out of the web endpoints while
# newt-web is down and re-adds it once it answers. `set +e` scopes to the
# subshell — an expected non-zero exit must not abort the loop under errexit.
# Bind per NEWT_WEB_BIND (D3).
(
    set +e
    while true; do
        su drake -c "RUST_LOG=info NEWT_WEB_BIND='${NEWT_WEB_BIND:-127.0.0.1:8880}' /usr/local/bin/newt-web"
        echo "entrypoint: newt-web exited ($?); respawning in 1s" >&2
        sleep 1
    done
) &

exec /usr/sbin/sshd -D -e -h "$dst/ssh_host_ed25519_key" -h "$dst/ssh_host_rsa_key"
