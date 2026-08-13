#!/usr/bin/env bash

# Run a Cargo test selection only after proving that the same selection lists
# at least one test. This prevents stale or misspelled filters from passing CI
# with Cargo's otherwise-successful "0 tests" result.

set -euo pipefail

cargo_bin=${CARGO_TEST_NONEMPTY_CARGO:-cargo}
required_test=""
if [[ "${1:-}" == "--require-test" ]]; then
    if [[ -z "${2:-}" ]]; then
        echo "error: --require-test needs an exact test name or suffix" >&2
        exit 2
    fi
    required_test=$2
    shift 2
fi

cargo_args=()
harness_args=()
found_separator=false

for arg in "$@"; do
    if [[ "$found_separator" == false && "$arg" == "--" ]]; then
        found_separator=true
        continue
    fi

    if [[ "$found_separator" == true ]]; then
        harness_args+=("$arg")
    else
        cargo_args+=("$arg")
    fi
done

if [[ "$found_separator" == false ]]; then
    echo "error: cargo-test-nonempty.sh requires '--' before libtest arguments" >&2
    exit 2
fi

list_output=""
if ! list_output=$("$cargo_bin" test "${cargo_args[@]}" -- "${harness_args[@]}" --list); then
    echo "error: cargo test selection could not be listed" >&2
    exit 1
fi
printf '%s\n' "$list_output"

test_count=$(printf '%s\n' "$list_output" | awk '
    { sub(/\r$/, "") }
    /: (test|benchmark)$/ { count++ }
    END { print count + 0 }
')

if [[ "$test_count" -eq 0 ]]; then
    printf 'error: cargo test selection matched zero tests: cargo test' >&2
    printf ' %q' "$@" >&2
    printf '\n' >&2
    exit 1
fi

if [[ -n "$required_test" ]]; then
    required_count=$(printf '%s\n' "$list_output" | awk -v required="$required_test" '
        { sub(/\r$/, "") }
        /: (test|benchmark)$/ {
            name = $0
            sub(/: (test|benchmark)$/, "", name)
            suffix = "::" required
            if (name == required || (length(name) > length(suffix) && \
                substr(name, length(name) - length(suffix) + 1) == suffix)) {
                count++
            }
        }
        END { print count + 0 }
    ')
    if [[ "$required_count" -ne 1 ]]; then
        printf 'error: required test %q matched %s listed tests; expected exactly one\n' \
            "$required_test" "$required_count" >&2
        exit 1
    fi
    printf 'verified required test %s\n' "$required_test"
fi

printf 'verified %s matching test(s)\n' "$test_count"
"$cargo_bin" test "${cargo_args[@]}" -- "${harness_args[@]}"

# Model: GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 16:43 EDT | Date: 2026-08-13
