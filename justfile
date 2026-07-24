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
#   just install-hooks — wire .githooks/ as the repo's hooks path and
#                        rewrite GitHub pushes to HTTPS (#276)

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

# Build against arbitrary agent-bridle + agent-mesh branches from sibling checkouts.
# This is a local, ephemeral test workflow:
#   1) temporarily check out the requested branches in ../agent-bridle and ../agent-mesh
#   2) relax version caps in this tree to "*" so local sibling crates can satisfy workspace deps
#   3) add temporary `[patch.crates-io]` for agent-bridle + agent-mesh-protocol
#   4) run `cargo build --workspace`
#   5) restore all modified manifests and original branches.
#
# Example:
#   just build-agent-stack main fix/mesh-proxy
[unix]
build-agent-stack AGENT_BRIDLE_BRANCH="main" AGENT_MESH_BRANCH="main":
    python3 scripts/build_agent_stack.py --agent-bridle-branch="{{AGENT_BRIDLE_BRANCH}}" --agent-mesh-branch="{{AGENT_MESH_BRANCH}}" "{{justfile_directory()}}"

# Install newt + newt-mcp-server release binaries to DEST (default: ~/bin).
# The rich TTY inline surface (issue #416) is a DEFAULT feature, so plain
# `just install` already gives you the rich editor — no flag needed.
# Override dest:           just install /usr/local/bin
# Extra features:          just install ~/bin some-crate/some-feature
# Lean / strip-down build: just install-lean   (lean crossterm box, wyvern tier)
install dest=`echo $HOME/bin` features="":
    cargo build --release --bin newt --bin newt-mcp-server {{ if features == "" { "" } else { "--features " + features } }}
    @just _place-binaries {{dest}}
    @echo "Installed: {{dest}}/newt  {{dest}}/newt-mcp-server {{ if features == "" { "" } else { "[features: " + features + "]" } }}"
    @case ":$PATH:" in *":{{dest}}:"*) ;; *) echo "Note: {{dest}} is not in PATH — add:  export PATH={{dest}}:\$PATH" ;; esac

# Install the LEAN strip-down build (hand-rolled crossterm lean box, no rich TTY
# surface) — the wyvern/headless tier. Same as `just install` but with
# `--no-default-features`.
install-lean dest=`echo $HOME/bin`:
    cargo build --release --no-default-features --bin newt --bin newt-mcp-server
    @just _place-binaries {{dest}}
    @echo "Installed (lean, no rich-tui): {{dest}}/newt  {{dest}}/newt-mcp-server"

# Place freshly built release binaries into DEST with a CLEAN inode and (on
# macOS) a fresh ad-hoc signature. Why not a plain `cp` over the old binary:
# on Apple Silicon, overwriting a running/launched binary in place invalidates
# the ad-hoc code signature the kernel's AMFI launch cache remembers, so the
# NEXT launch dies with "Killed: 9" even though `codesign -v` still passes on
# disk. Removing first gives a new inode; re-signing ad-hoc clears the cache.
# `codesign` exists only on macOS, so the re-sign is a no-op elsewhere.
#
# Resolve the copy source through `cargo metadata`: unlike
# ${CARGO_TARGET_DIR:-target}, this also honors `[build] target-dir` in workspace
# or user Cargo config. That prevents installing a stale local binary when the
# fresh build landed in a configured shared target directory.
_place-binaries dest:
    mkdir -p {{dest}}
    rm -f {{dest}}/newt {{dest}}/newt-mcp-server
    target_dir="$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')"; \
        cp "$target_dir/release/newt" {{dest}}/newt; \
        cp "$target_dir/release/newt-mcp-server" {{dest}}/newt-mcp-server
    @if command -v codesign >/dev/null 2>&1; then codesign --force --sign - {{dest}}/newt {{dest}}/newt-mcp-server && echo "re-signed ad-hoc (macOS AMFI)"; fi

# Remove the binaries `just install` placed in DEST (default: ~/bin) — the
# inverse of `just install`, so you can guarantee which build is on PATH.
uninstall dest=`echo $HOME/bin`:
    rm -f {{dest}}/newt {{dest}}/newt-mcp-server
    @echo "Removed newt + newt-mcp-server from {{dest}}"

# Remove all Cargo build artefacts (force a clean rebuild / free disk space).
clean:
    cargo clean

# --- Test ---

# Run every test in the workspace.
#
# `--features newt-data/kernel` turns on the Phase 21.3 live-kernel transport so
# newt-data's kernel tests (the pure iopub accumulator + the mock-websocket
# `rest.rs` suite) actually run. The feature is off by default (it pulls a
# websocket/TLS stack), so without this flag those tests are silently skipped.
# A workspace-level feature flag only affects crates that declare it; the rest
# build unchanged.
test:
    cargo test --workspace --features newt-data/kernel

# --- Lint & format ---

# Apply rustfmt to the whole workspace.
fmt:
    cargo fmt --all

# Lint with the workspace's clippy config; zero-warnings gate.
# `--features newt-data/kernel` so the Phase 21.3 kernel code + tests are linted.
lint:
    cargo clippy --workspace --all-targets --features newt-data/kernel -- -D warnings

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
    cargo clippy --workspace --all-targets --features newt-data/kernel -- -D warnings || rc=1
    cargo test --workspace --features newt-data/kernel || rc=1
    # rich-tui (issue #416) is now a DEFAULT feature (amphibian: comfortable by
    # default, strip for the water), so the workspace clippy/test above already
    # cover it. Guard the LEAN strip-down build instead — the wyvern/headless
    # tier (`--no-default-features`) must still compile + lint clean.
    cargo clippy -p newt-agent --no-default-features --all-targets -- -D warnings || rc=1
    exit $rc

[windows]
check:
    $rc = 0; cargo fmt --all -- --check; if ($LASTEXITCODE -ne 0) { $rc = 1 }; cargo clippy --workspace --all-targets --features newt-data/kernel -- -D warnings; if ($LASTEXITCODE -ne 0) { $rc = 1 }; cargo test --workspace --features newt-data/kernel; if ($LASTEXITCODE -ne 0) { $rc = 1 }; cargo clippy -p newt-agent --no-default-features --all-targets -- -D warnings; if ($LASTEXITCODE -ne 0) { $rc = 1 }; exit $rc

# README staleness lint (#1023): the README states invariants and links to
# live sources of truth — it must never carry operator-local paths, cluster
# hostnames, or time-anchored phrasing that rots. Candidate release-gate step.
[unix]
readme-check:
    #!/usr/bin/env bash
    set -uo pipefail
    rc=0
    # Forbidden: private/local paths, redacted infra, staleness vocabulary.
    patterns=(
        '~/workspaces' '~/\.claude' 'REDACTED'
        'for now' 'coming soon' 'recently' 'planned as a follow-up'
        'now includes' 'will soon'
    )
    for p in "${patterns[@]}"; do
        if grep -Einq "$p" README.md; then
            echo "readme-check: forbidden pattern '$p' in README.md:" >&2
            grep -Ein "$p" README.md >&2
            rc=1
        fi
    done
    # Every relative markdown link target must exist in the repo.
    while IFS= read -r target; do
        [ -e "$target" ] || { echo "readme-check: dead link ./$target" >&2; rc=1; }
    done < <(grep -oE '\]\(\./[^)#]+' README.md | sed 's/](\.\///')
    exit $rc

# Build + test the out-of-workspace newt-mesh crate. Requires the
# sibling `../agent-mesh/` checkout. Not run by `just check` /
# CI — see docs/decisions/mesh_integration.md.
#
# Mirrors the mesh-integration per-PR gate: `cargo test` skips the
# `#[ignore]`d live-transport round-trip tests (flaky pending agent-mesh
# #61/#62, #1190). To exercise the live transport locally, append
# `-- --include-ignored` (that's the nightly/dispatch lane).
check-mesh:
    cargo fmt --manifest-path newt-mesh/Cargo.toml -- --check
    cargo clippy --manifest-path newt-mesh/Cargo.toml --all-targets -- -D warnings
    cargo test --manifest-path newt-mesh/Cargo.toml

# --- Security audit (A1) ---

# Security advisory gate. Fails on any RustSec advisory NOT in the tracked
# ignore-list (.cargo/audit.toml — pre-existing transitive advisories, issue
# #656). PIPELINE PARITY: mirrors the "Security audit (cargo-audit)" job in
# .github/workflows/ci.yml and the .githooks/pre-push hook. Local best-effort:
# skips (with a note) if cargo-audit isn't installed — CI is the hard gate.
audit:
    #!/usr/bin/env bash
    set -uo pipefail
    if ! cargo audit --version >/dev/null 2>&1; then
        echo "[audit] cargo-audit not installed — skipping locally (CI enforces it)."
        echo "        install: cargo install cargo-audit"
        exit 0
    fi
    cargo audit

# --- MSRV (A2) ---

# MSRV gate: the workspace must compile on the declared rust-version (1.88,
# [workspace.package] in Cargo.toml). PIPELINE PARITY: mirrors the "MSRV
# (Rust 1.88)" job in .github/workflows/ci.yml and the .githooks/pre-push hook.
# Local best-effort: needs rustup + the 1.88 toolchain; skips (with a note)
# otherwise — CI is the hard gate.
msrv:
    #!/usr/bin/env bash
    set -uo pipefail
    if ! rustup toolchain list 2>/dev/null | grep -q '^1\.88'; then
        echo "[msrv] rust 1.88 toolchain not installed — skipping locally (CI enforces it)."
        echo "        install: rustup toolchain install 1.88"
        exit 0
    fi
    cargo +1.88 check --workspace

# --- Coverage ---
#
# Coverage is gated at 80% workspace-wide from Step 0.3 onward. The
# `cov` recipe is for local exploration (HTML report under
# target/llvm-cov/html/index.html); `cov-ci` is what the pipeline runs.

# Generate an HTML coverage report for human review.
# `--features newt-data/kernel` so the Phase 21.3 kernel module + its tests are
# instrumented (otherwise the kernel code is built — via newt-mcp-data — but its
# own tests never run, falsely tanking the kernel files' coverage).
cov:
    cargo llvm-cov --workspace --features newt-data/kernel --html
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
    # Pin the target dir to THIS checkout. A shared/inherited CARGO_TARGET_DIR
    # is how a sibling worktree's profraw leaks into the report (0%-coverage
    # ghost files tanking the total — the CLAUDE.md shared-target hazard).
    # Pinning kills the contamination at the source. The earlier fix — adding
    # `\.worktrees/` to the ignore regex — matched against ABSOLUTE paths, so
    # any checkout living under a `.worktrees/` directory (the workspace's own
    # agent-worktree convention) filtered out its ENTIRE repo and failed the
    # gate at -%. CI (clean checkout, no shared target) never saw either bug.
    export CARGO_TARGET_DIR="$(pwd)/target"
    cargo llvm-cov --workspace --features newt-data/kernel --no-report
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
    $floor = 80; $env:CARGO_TARGET_DIR = Join-Path (Get-Location) 'target'; cargo llvm-cov --workspace --features newt-data/kernel --no-report; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; cargo llvm-cov report --lcov --output-path lcov.info --ignore-filename-regex 'pyo3_module\.rs$'; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; $summary = cargo llvm-cov report --summary-only --ignore-filename-regex 'pyo3_module\.rs$'; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; $summary; $total = $summary | Where-Object { $_ -match '^TOTAL\s+' } | Select-Object -First 1; if (-not $total) { Write-Error 'ERROR: could not parse line coverage from cargo-llvm-cov summary'; exit 1 }; $cols = $total -split '\s+'; $line_cov = [double]($cols[9].TrimEnd('%')); Write-Output "measured line coverage: $line_cov% (floor: $floor%)"; if ($line_cov -lt $floor) { Write-Error "ERROR: workspace line coverage $line_cov% is below the $floor% floor"; exit 1 }; Write-Output "coverage gate OK: $line_cov% >= $floor%"

# --- OCAP honesty gate ---
#
# `just ocap-check` — the OCAP deviation honesty gate (the authority-plane analog of
# `cov-ci`). It ratchets DEVIATIONS CLOSED instead of coverage UP: validates the
# deviation register (docs/security/ocap-deviations.md) is complete, and asserts every
# `OCAP-DANGER:<id>` site in the tree carries its `OCAP-GATE:<id>` fail-closed gate
# unless that deviation is CLOSED. So a dangerous capability cannot be added without
# either closing its invariant or wiring its gate. See
# docs/design/centaur-swarm-architecture.md and docs/security/ocap-deviations.md.
# Standalone for now (pure-Python, no cargo deps); wire into the pre-push hook + CI
# once the captured-shell / credential code begins to land.
ocap-check:
    python3 scripts/ocap_check.py

#
# `main`/release MUST stay on agent-bridle's `feat/step-up-decision-mvp` branch
# (advanced from `feat/stub-shell` on 2026-06-19, newt#497 — it adds the `step_up`
# Gate while staying stub): it carries NO brush git deps, so the workspace
# publishes to crates.io (see the
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

# Run the live BAT/UAT eval against the gnuc home box's Ollama + a test LLM —
# the hand-run equivalent of the `eval-live.yml` weekly CI job. Override the
# model/host, and filter to BAT (L1) or UAT (L2,L3):
#
#   just eval-live                                  # default test model @ gnuc
#   just eval-live qwen2.5-coder:7b                 # pick a model
#   just eval-live llama3.1:8b http://REDACTED-HOST:11434 --difficulty L2,L3
eval-live MODEL="qwen2.5-coder:7b" HOST="http://gnuc:11434" *ARGS:
    cargo build --release --bin newt
    cargo build --release --bin newt-eval
    OLLAMA_HOST="{{HOST}}" NEWT_DEFAULT_MODEL="{{MODEL}}" \
        ./target/release/newt-eval run --mode live --coder --model "{{MODEL}}" {{ARGS}}

# --- Hook installation ---

# Point this repo at .githooks/ for pre-push gating, and rewrite GitHub
# pushes to HTTPS. Idempotent — safe to re-run.
#
# Why the pushInsteadOf rewrite (#276): for SSH remotes git opens the
# connection and reads the advertised refs BEFORE the pre-push hook runs,
# then sends the pack over that SAME connection afterwards. Our gate takes
# ~10 minutes — longer than GitHub's SSH idle timeout — so every SSH push
# passes the gate and then dies with SIGPIPE (141) writing the pack into a
# dead connection. Over HTTPS the pack upload is a fresh request after the
# hook, so gate duration is irrelevant. The rewrite is REPO-LOCAL and
# affects pushes only; fetches keep their configured (SSH) URL.
#
# HTTPS push auth comes from a git credential helper — `gh auth setup-git`
# configures gh's. The recipe checks for one and warns (non-fatally) if
# missing; it deliberately does NOT run `gh auth setup-git` itself, since
# that writes to the user's *global* git config, which a repo recipe has
# no business touching.
[unix]
install-hooks:
    #!/usr/bin/env bash
    set -euo pipefail
    git config core.hooksPath .githooks
    git config url."https://github.com/".pushInsteadOf "git@github.com:"
    echo "core.hooksPath -> .githooks (pre-push gate wired)"
    echo "pushes to git@github.com:* rewritten to https://github.com/* (#276)"
    if git config --get-urlmatch credential.helper https://github.com >/dev/null 2>&1; then
        echo "credential helper for https://github.com: OK"
    elif command -v gh >/dev/null 2>&1; then
        echo "WARNING: no git credential helper configured for https://github.com." >&2
        echo "         HTTPS pushes will prompt for a password (GitHub rejects those)." >&2
        echo "         Run once: gh auth setup-git" >&2
    else
        echo "WARNING: no git credential helper configured for https://github.com" >&2
        echo "         and 'gh' is not installed. Install GitHub CLI and run" >&2
        echo "         'gh auth setup-git', or configure another credential helper." >&2
    fi

[windows]
install-hooks:
    git config core.hooksPath .githooks; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; git config url."https://github.com/".pushInsteadOf "git@github.com:"; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; Write-Output "core.hooksPath -> .githooks (pre-push gate wired)"; Write-Output "pushes to git@github.com:* rewritten to https://github.com/* (#276)"; git config --get-urlmatch credential.helper https://github.com *> $null; if ($LASTEXITCODE -ne 0) { Write-Warning "no git credential helper for https://github.com - run: gh auth setup-git" }
