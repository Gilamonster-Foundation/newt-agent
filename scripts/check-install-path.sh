#!/bin/sh
set -eu

canonical() {
    target=$1

    if [ -x /bin/realpath ]; then
        /bin/realpath "$target"
        return
    fi
    if [ -x /usr/bin/realpath ]; then
        /usr/bin/realpath "$target"
        return
    fi
    if [ -x /usr/bin/readlink ]; then
        /usr/bin/readlink -f "$target" 2>/dev/null || /usr/bin/readlink "$target"
        return
    fi

    cd "$(dirname "$target")"
    printf '%s\n' "$(pwd -P)/$(basename "$target")"
}

install_warn() {
    install_dir=$1
    installed="$install_dir/newt"

    [ -x "$installed" ] || return 0

    active_path="$(command -v newt 2>/dev/null || true)"
    [ -n "$active_path" ] || return 0
    [ -x "$active_path" ] || return 0

    installed_real=$(canonical "$installed")
    active_real=$(canonical "$active_path")

    if [ "$active_real" != "$installed_real" ]; then
        printf '%s\n' \
            "Warning: another newt shadows this installation on PATH." \
            "  active: $active_path" \
            "  installed: $installed" \
            "Put $install_dir first in PATH, then refresh the command cache:" \
            "  zsh:  rehash" \
            "  bash: hash -r"
    fi
}

self_test() {
    original_path=$PATH
    d=$(mktemp -d)
    trap 'rm -rf "$d"' EXIT HUP INT TERM

    mkdir -p "$d/installed" "$d/link" "$d/shadow"
    printf '#!/bin/sh\n' >"$d/installed/newt"
    printf '#!/bin/sh\n' >"$d/shadow/newt"
    chmod +x "$d/installed/newt" "$d/shadow/newt"
    ln -s "$d/installed/newt" "$d/link/newt"

    a="$(PATH="$d/installed:$d/link:$d/shadow:$original_path" "$0" "$d/installed")"
    a="$a$(PATH="$d/link:$d/installed:$d/shadow:$original_path" "$0" "$d/installed")"
    b="$(PATH="$d/shadow:$d/installed:$d/link:$original_path" "$0" "$d/installed")"

    [ -z "$a" ] || { echo "FAIL: install/symlink reported as shadowed" >&2; return 1; }
    case "$b" in *"$d/shadow/newt"*"$d/installed/newt"*rehash*"hash -r"*) :;; *)
        echo "FAIL: shadow warning omitted paths" >&2
        return 1
        ;;
    esac

    echo "install PATH self-test: OK"
}

if [ "${1:-}" = --self-test ]; then
    self_test
else
    install_warn "$1"
fi
