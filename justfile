# newt-agent — task runner
#
# PIPELINE PARITY: This justfile is the local mirror of the CI pipeline
# defined at .github/workflows/ci.yml (added in Step 0.3). The pre-push
# hook at .githooks/pre-push (added in Step 0.2) calls `just check` and
# `just cov-ci` — keep all three in lock-step.
#
# Quick reference:
#   just            — list available recipes (default)
#   just check      — full local gate (fmt + clippy + test)
#   just cov        — local coverage with HTML report
#   just cov-ci     — coverage with 80% gate, lcov output (CI mode)
#   just install-hooks — wire .githooks/ as the repo's hooks path

set windows-shell := ["powershell.exe", "-NoProfile", "-Command"]

default:
    @just --list

# --- Build ---

# Default debug build for the whole workspace.
build:
    cargo build --workspace

# Optimized release build for the whole workspace.
release:
    cargo build --workspace --release

# Install newt + newt-mcp-server release binaries to DEST (default: ~/bin).
# Override: just install /usr/local/bin
install dest=`echo $HOME/bin`:
    cargo build --release --bin newt --bin newt-mcp-server
    mkdir -p {{dest}}
    cp target/release/newt {{dest}}/newt
    cp target/release/newt-mcp-server {{dest}}/newt-mcp-server
    @echo "Installed: {{dest}}/newt  {{dest}}/newt-mcp-server"
    @case ":$PATH:" in *":{{dest}}:"*) ;; *) echo "Note: {{dest}} is not in PATH — add:  export PATH={{dest}}:\$PATH" ;; esac

# Remove all Cargo build artefacts (force a clean rebuild / free disk space).
clean:
    cargo clean

# --- Test ---

# Run every test in the workspace.
test:
    cargo test --workspace

# --- Lint & format ---

# Apply rustfmt to the whole workspace.
fmt:
    cargo fmt --all

# Lint with the workspace's clippy config; zero-warnings gate.
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Regenerate Cargo.lock from scratch (authoritative resolution).
# Run this after adding or changing dependencies so the lock file matches
# what CI's `--locked` flag validates against.
lock:
    cargo generate-lockfile

# fmt-check, lint, and test — the local equivalent of CI.
# PIPELINE PARITY: must match .github/workflows/ci.yml. Runs all three even if
# an earlier one fails (a fmt failure must not mask a clippy failure), matching
# the `if: always()` on CI's clippy step; exits non-zero if any failed.
[unix]
check:
    #!/usr/bin/env bash
    set -uo pipefail
    rc=0
    cargo fmt --all -- --check || rc=1
    cargo clippy --workspace --all-targets -- -D warnings || rc=1
    cargo test --workspace || rc=1
    exit $rc

[windows]
check:
    $rc = 0; cargo fmt --all -- --check; if ($LASTEXITCODE -ne 0) { $rc = 1 }; cargo clippy --workspace --all-targets -- -D warnings; if ($LASTEXITCODE -ne 0) { $rc = 1 }; cargo test --workspace; if ($LASTEXITCODE -ne 0) { $rc = 1 }; exit $rc

# Build + test the out-of-workspace newt-mesh crate. Requires the
# sibling `../agent-mesh/` checkout. Not run by `just check` /
# CI — see docs/decisions/mesh_integration.md.
check-mesh:
    cargo fmt --manifest-path newt-mesh/Cargo.toml -- --check
    cargo clippy --manifest-path newt-mesh/Cargo.toml --all-targets -- -D warnings
    cargo test --manifest-path newt-mesh/Cargo.toml

# --- Coverage ---
#
# Coverage is gated at 80% workspace-wide from Step 0.3 onward. The
# `cov` recipe is for local exploration (HTML report under
# target/llvm-cov/html/index.html); `cov-ci` is what the pipeline runs.

# Generate an HTML coverage report for human review.
cov:
    cargo llvm-cov --workspace --html
    @echo "HTML report at target/llvm-cov/html/index.html"

# CI-mode coverage: emit lcov + enforce the current floor.
# PIPELINE PARITY: must match the coverage job in .github/workflows/ci.yml.
#
# The floor RATCHETS UP — never down — as the codebase grows. The
# roadmap targets 80% workspace-wide; bootstrap baseline was 15%
# (Step 0.3 landed the workflow before there was enough code to
# justify 80%), ratcheted to 75% in the stdio-safety PR, and to the
# roadmap's 80% in the tuning-writeback PR.
# Each PR that adds tests should also bump this threshold higher;
# each PR that adds untested code will fail the gate.
#
# pyo3_module.rs files are excluded: they only compile under the
# `pyo3` cargo feature (umbrella `newt-agent-py` crate turns it on)
# and have their own pytest suite at newt-agent-py/tests/. Counting
# them as zero-coverage in the default-feature build would falsely
# tank the workspace number.
#
# Why we don't rely on cargo-llvm-cov's --fail-under-lines:
# issue #100 caught it silently exit-0'ing on a sub-floor commit
# (cargo-llvm-cov 0.8.5 ignores --fail-under-lines when --lcov
# --output-path is set, and `report --summary-only --fail-under-lines`
# is also unreliable in that version). We instead parse the TOTAL
# line from `report --summary-only` and gate in shell — deterministic,
# version-independent, and the measured percentage is always visible.
[unix]
cov-ci:
    #!/usr/bin/env bash
    set -euo pipefail
    floor=80
    cargo llvm-cov --workspace --no-report
    cargo llvm-cov report --lcov --output-path lcov.info --ignore-filename-regex 'pyo3_module\.rs$'
    summary=$(cargo llvm-cov report --summary-only --ignore-filename-regex 'pyo3_module\.rs$')
    echo "$summary"
    # TOTAL row columns: regions missed cov% funcs missed cov% lines missed cov% ...
    # Line coverage is column 10 (3rd "Cover" column).
    line_cov=$(printf '%s\n' "$summary" | awk '$1 == "TOTAL" { gsub("%", "", $10); print $10 }')
    if [ -z "${line_cov:-}" ]; then
        echo "ERROR: could not parse line coverage from cargo-llvm-cov summary" >&2
        exit 1
    fi
    echo "measured line coverage: ${line_cov}% (floor: ${floor}%)"
    if awk -v cov="$line_cov" -v floor="$floor" 'BEGIN { exit !(cov + 0 < floor + 0) }'; then
        echo "ERROR: workspace line coverage ${line_cov}% is below the ${floor}% floor" >&2
        exit 1
    fi
    echo "coverage gate OK: ${line_cov}% >= ${floor}%"

[windows]
cov-ci:
    $floor = 80; cargo llvm-cov --workspace --no-report; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; cargo llvm-cov report --lcov --output-path lcov.info --ignore-filename-regex 'pyo3_module\.rs$'; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; $summary = cargo llvm-cov report --summary-only --ignore-filename-regex 'pyo3_module\.rs$'; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; $summary; $total = $summary | Where-Object { $_ -match '^TOTAL\s+' } | Select-Object -First 1; if (-not $total) { Write-Error 'ERROR: could not parse line coverage from cargo-llvm-cov summary'; exit 1 }; $cols = $total -split '\s+'; $line_cov = [double]($cols[9].TrimEnd('%')); Write-Output "measured line coverage: $line_cov% (floor: $floor%)"; if ($line_cov -lt $floor) { Write-Error "ERROR: workspace line coverage $line_cov% is below the $floor% floor"; exit 1 }; Write-Output "coverage gate OK: $line_cov% >= $floor%"

# --- agent-bridle shell toggle (publishable stub vs. real confined shell) ---
#
# `main`/release MUST stay on agent-bridle's `feat/stub-shell` branch: it
# carries NO brush git deps, so the workspace publishes to crates.io (see the
# [patch.crates-io] block in Cargo.toml, and #206 / #208). The real
# Caveats-confined shell lives on agent-bridle `main`, but it pulls the brush
# git fork — which crates.io forbids in any form (even optional / feature-gated)
# — so it can only ever be a DEV-ONLY override, never committed to `main`.
#
# A cargo `--features` flag can't express this (it would swap a dependency
# *source*, not toggle a feature), so the lever is this recipe pair plus a
# guard. `shell-real` flips the local patch onto the real shell; `shell-stub`
# flips it back; `shell-check` (run by the pre-push hook + CI) fails if the
# dev override ever reaches `main`.

# Switch the LOCAL build to the real confined brush shell (agent-bridle `main`).
# DEV ONLY — the resulting Cargo.toml/Cargo.lock must NOT be committed (it
# reintroduces the brush git dep and breaks crates.io publish). Run
# `just shell-stub` before you commit. Rebuild after this to pick it up.
shell-real:
    # Portable across GNU and BSD/macOS sed: `[{]` (not `\{`, which BSD ERE
    # reads as an interval) and a temp-file rewrite (not `sed -i`, whose suffix
    # arg differs between GNU and BSD). See PR #238 followup.
    sed -E 's|(agent-bridle[a-z-]*[[:space:]]*= [{] git = "[^"]*agent-bridle", branch = )"feat/stub-shell"|\1"main"|' Cargo.toml > Cargo.toml.shelltmp && mv Cargo.toml.shelltmp Cargo.toml
    @echo "⚠️  agent-bridle → REAL brush shell (agent-bridle main). DEV ONLY."
    @echo "⚠️  Do NOT commit Cargo.toml / Cargo.lock — run 'just shell-stub' first."
    @echo "   Now rebuild: cargo build --workspace"

# Switch back to the publishable stub shell (the release / main default).
shell-stub:
    # Portable sed (see `shell-real` for why `[{]` + temp-file, not `sed -i -E`).
    sed -E 's|(agent-bridle[a-z-]*[[:space:]]*= [{] git = "[^"]*agent-bridle", branch = )"main"|\1"feat/stub-shell"|' Cargo.toml > Cargo.toml.shelltmp && mv Cargo.toml.shelltmp Cargo.toml
    @echo "agent-bridle → stub shell (publishable). Safe to commit."
    @echo "   Now rebuild: cargo build --workspace"

# Guard: fail if the dev (real-shell) override is present in Cargo.toml.
# Run by the pre-push hook and mirrored inline in CI.
# PIPELINE PARITY: the same grep lives in .github/workflows/ci.yml (lint job).
shell-check:
    #!/usr/bin/env bash
    set -euo pipefail
    if grep -Eq 'agent-bridle[a-z-]*[[:space:]]*= [{] git = "[^"]*agent-bridle", branch = "main"' Cargo.toml; then
        echo "ERROR: [patch.crates-io] points agent-bridle at 'main' (real brush shell)." >&2
        echo "       That build cannot publish to crates.io. Run 'just shell-stub'" >&2
        echo "       before committing/pushing — see the shell-toggle note in the justfile." >&2
        exit 1
    fi
    echo "shell guard OK: agent-bridle patch is on the publishable stub branch."

# --- Evaluation ---

# Run the end-to-end eval suite against a real local Ollama.
#
# Mock-mode runs in CI via `cargo test -p newt-eval --test mock_e2e`;
# `just eval` is the opt-in live-mode entry point developers use to
# track regressions against an actual model. Pass extra `newt-eval run`
# args after the recipe, e.g.:
#
#   just eval                           # all bundled cases
#   just eval --case 001                # only the rename-function case
#   just eval --model llama3.1:8b       # pick a specific model
eval *ARGS:
    cargo build --release --bin newt
    cargo build --release --bin newt-eval
    ./target/release/newt-eval run --mode live {{ARGS}}

# --- Hook installation ---

# Point this repo at .githooks/ for pre-push gating.
# Idempotent — safe to re-run.
install-hooks:
    git config core.hooksPath .githooks
    @echo "core.hooksPath -> .githooks (.githooks/pre-push lands in Step 0.2)"
