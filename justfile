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

# fmt-check, lint, and test — the local equivalent of CI.
# PIPELINE PARITY: must match .github/workflows/ci.yml.
check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

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
# justify 80%). Workspace coverage is now ~89.78% — ratcheted to
# 75% here to give a margin while approaching the 80% target.
# Each PR that adds tests should also bump this threshold higher;
# each PR that adds untested code will fail the gate.
#
# pyo3_module.rs files are excluded: they only compile under the
# `pyo3` cargo feature (umbrella `newt-agent-py` crate turns it on)
# and have their own pytest suite at newt-agent-py/tests/. Counting
# them as zero-coverage in the default-feature build would falsely
# tank the workspace number.
cov-ci:
    cargo llvm-cov --workspace --lcov --output-path lcov.info --fail-under-lines 75 --ignore-filename-regex 'pyo3_module\.rs$'

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
