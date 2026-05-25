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

default:
    @just --list

# --- Build ---

# Default debug build for the whole workspace.
build:
    cargo build --workspace

# Optimized release build for the whole workspace.
release:
    cargo build --workspace --release

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

# fmt-check, lint, and test — the local equivalent of CI.
# PIPELINE PARITY: must match .github/workflows/ci.yml.
check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

# --- Coverage ---
#
# Coverage is gated at 80% workspace-wide from Step 0.3 onward. The
# `cov` recipe is for local exploration (HTML report under
# target/llvm-cov/html/index.html); `cov-ci` is what the pipeline runs.

# Generate an HTML coverage report for human review.
cov:
    cargo llvm-cov --workspace --html
    @echo "HTML report at target/llvm-cov/html/index.html"

# CI-mode coverage: emit lcov + enforce the 80% floor.
# PIPELINE PARITY: must match the coverage job in .github/workflows/ci.yml.
cov-ci:
    cargo llvm-cov --workspace --lcov --output-path lcov.info --fail-under-lines 80

# --- Hook installation ---

# Point this repo at .githooks/ for pre-push gating.
# Idempotent — safe to re-run.
install-hooks:
    git config core.hooksPath .githooks
    @echo "core.hooksPath -> .githooks (.githooks/pre-push lands in Step 0.2)"
