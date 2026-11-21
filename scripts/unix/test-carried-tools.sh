#!/usr/bin/env bash
# Run the staged carried command pack (#3151): the Unix counterpart of
# scripts/windows/Test-CarriedTools.ps1. Each tool runs with ONLY the pack on
# PATH and an otherwise empty environment, so a pass shows the pack stands on
# its own, including `grep`, which needs the adjacent ripgrep.
#
#   scripts/unix/test-carried-tools.sh <tools-directory>
set -uo pipefail

tools=${1:?usage: test-carried-tools.sh <tools-directory>}
[ -d "$tools" ] || { echo "test-carried-tools: no such directory: $tools" >&2; exit 2; }
tools=$(cd "$tools" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
fixture="$work/fixture"
mkdir "$fixture"
printf 'alpha\nbeta\nalpha beta\n' >"$fixture/data.txt"
printf 'b\na\na\n' >"$fixture/repeat.txt"
checks=0

fail() { echo "FAIL: $*" >&2; exit 1; }

# check <stdin> <expected stdout> <expected exit> <tool> [args...]
check() {
  local input=$1 expected=$2 code=$3 tool=$4 out rc
  shift 4
  out=$(cd "$fixture" && printf '%s' "$input" |
    env -i PATH="$tools" HOME="$work" "$tools/$tool" "$@" 2>"$work/err"; echo "rc=$?")
  rc=${out##*rc=}
  out=${out%rc=*}
  [ "$rc" = "$code" ] || fail "$tool $* exited $rc, expected $code: $(cat "$work/err")"
  [ "$out" = "$expected" ] || fail "$tool $* printed <$out>, expected <$expected>"
  checks=$((checks + 1))
}

check "" $'b\na\na\n' 0 cat repeat.txt
check "" $'b\n' 0 head -1 repeat.txt
check "" $'a\n' 0 tail -1 repeat.txt
check $'a\nb\n' $'2\n' 0 wc -l
check "" $'data.txt\nrepeat.txt\n' 0 ls
check "" $'a\na\nb\n' 0 sort repeat.txt
check $'a\na\nb\n' $'a\nb\n' 0 uniq
check $'b\na\n' $'B\nA\n' 0 tr a-z A-Z
check "" $'a\nb\na\n' 0 cut -c1 data.txt
check "" $'carried echo\n' 0 echo carried echo
check "" "" 0 mkdir made
check "" "" 0 cp data.txt made/copy.txt
check "" "" 0 mv made/copy.txt made/moved.txt
check "" $'alpha\nbeta\nalpha beta\n' 0 cat made/moved.txt
check "" "" 0 rm made/moved.txt
check "" "" 0 ls made
check "" "$tools/cat"$'\n' 0 which cat

# #3151: the GNU `date -d` a BSD date lacks, on the form git prints.
check "" $'1794582000\n' 0 date -u -d '2026-11-13 10:00:00 -0500' +%s
check "" $'1970-01-01\n' 0 date -u -d @0 +%F

# grep drives the adjacent ripgrep: the supported set, then -P (#3151).
check "" $'alpha\nalpha beta\n' 0 grep alpha data.txt
check "" $'2\n' 0 grep -c alpha data.txt
check "" "" 1 grep zzz data.txt
check $'x  y [origin/a: gone]\n' $'[origin/a: gone]\n' 0 grep -oP '^\S+\s+\S+\s+\K\[origin/\S+: gone\]'
check $'refs/heads/x\nremotes/y\nHEAD\n' $'refs/heads/x\n' 0 grep -vP 'remotes/|HEAD'
check $'foo=1\nbar=2\n' $'1\n' 0 grep -oP '(?<=foo=)\d'

# An option outside the supported set is refused with the supported set, exit 2.
refusal=$(cd "$fixture" && env -i PATH="$tools" HOME="$work" "$tools/grep" -q alpha data.txt 2>&1)
rc=$?
[ "$rc" = 2 ] || fail "grep -q exited $rc, expected 2"
case $refusal in *"supported:"*) ;; *) fail "grep -q gave no supported set: $refusal";; esac
checks=$((checks + 1))

echo "PASS: $checks packaged native command checks"
