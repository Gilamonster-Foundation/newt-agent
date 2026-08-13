#!/usr/bin/env bash

# Self-test the zero-match and exact-required-test guard without compiling the
# workspace. A fake Cargo executable emits deterministic libtest `--list`
# output, so the assertions run identically on Linux, macOS, and Git Bash.

set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
fake_cargo() {
if [[ " $* " == *" --list "* ]]; then
    printf '%b' "${FAKE_CARGO_LIST:-}"
fi
}
export -f fake_cargo

guard="$script_dir/cargo-test-nonempty.sh"
export CARGO_TEST_NONEMPTY_CARGO=fake_cargo

FAKE_CARGO_LIST=$'suite::required_case: test\r\nsuite::other: test\r\n' \
    bash "$guard" --require-test required_case -p fake -- --ignored >/dev/null

if FAKE_CARGO_LIST=$'suite::other: test\n' \
    bash "$guard" --require-test required_case -p fake -- --ignored >/dev/null 2>&1; then
    echo "error: missing required test unexpectedly passed" >&2
    exit 1
fi

if FAKE_CARGO_LIST=$'one::required_case: test\ntwo::required_case: test\n' \
    bash "$guard" --require-test required_case -p fake -- --ignored >/dev/null 2>&1; then
    echo "error: ambiguous required-test suffix unexpectedly passed" >&2
    exit 1
fi

if FAKE_CARGO_LIST='' bash "$guard" -p fake -- --ignored >/dev/null 2>&1; then
    echo "error: zero-test selection unexpectedly passed" >&2
    exit 1
fi

echo "cargo-test-nonempty self-test passed"

# Model: GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 16:58 EDT | Date: 2026-08-13
