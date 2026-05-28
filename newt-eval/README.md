# newt-eval — end-to-end evaluation framework for the newt worker

`newt-eval` is the scorecard for `newt worker`. It answers:

> Does this version of the worker actually produce useful patches?

It is also the dogfood substrate for the drake-arbiter scoring pattern
— the same test-case + evaluators + scorecard shape the arbiter uses
to grade swarm candidates.

## Two modes

| Mode | Trigger | What it talks to | CI gate? |
|------|---------|-------------------|----------|
| Mock | `cargo test -p newt-eval --test mock_e2e` | `wiremock` stand-in for Ollama; the real `newt` binary drives ACP | yes — runs in `just check` |
| Live | `just eval` (alias for `newt-eval run --mode live`) | A real local Ollama via `OLLAMA_HOST` or the worker's default endpoint list | no — opt-in developer tool |

Mock mode is fully deterministic — every case ships with a canned diff
that the wiremock returns verbatim. The evaluators then verify the
worker did the right thing with that response (parsed it, applied it
to the workspace, captured the resulting git diff, returned a
well-formed `TaskReply`).

Live mode is for comparing models / catching regressions: the worker
runs against your real Ollama and the same evaluators grade whatever
the model returns. Weak models will legitimately fail — that's the
point.

## Running

```bash
# Mock e2e (runs in CI):
cargo test -p newt-eval --test mock_e2e

# List the bundled cases:
cargo run -q --bin newt-eval -- list-cases

# Live: all cases against discovered Ollama:
just eval

# Live: one case, specific model:
just eval --case 001 --model llama3.1:8b
```

Exit codes from `newt-eval run`:

- `0` — every case passed every evaluator
- `2` — ran cleanly but at least one case failed
- `1` — hard error (worker not found, cases dir missing, etc.)

## The five evaluators

| Name | What it checks |
|------|----------------|
| `diff_nonempty` | `reply.diff` is non-empty AND `!reply.empty_diff` |
| `diff_applies`  | Copy baseline to a tempdir, `git apply --check` accepts the diff |
| `rust_compiles` | `cargo check` on the post-worker workspace (Rust cases only) |
| `tests_pass`    | `cargo test` on the post-worker workspace (Rust cases with `#[test]` only) |
| `pattern_match` | At least one of `expected_patterns` regex matches the captured diff |

Language-specific evaluators auto-skip non-Rust cases with a clear note.
`tests_pass` also skips Rust cases that have no `#[test]` anywhere so
trivial cases don't pay the cargo-test bill.

## Adding a new case

A case is a directory under `newt-eval/cases/NNN-name/` with two parts:

1. `case.toml` — metadata + mock response.
2. `workspace/` — the initial filesystem state, copied verbatim into a
   tempdir at the start of each run.

```toml
# newt-eval/cases/006-your-case/case.toml
name = "006-your-case"
description = "One-line summary"
language = "rust"
prompt = """
Multi-line instruction to the worker. End with "Respond with a unified
diff only." so the model knows what shape to return.
"""

evaluators = [
    "diff_nonempty",
    "diff_applies",
    "rust_compiles",
    "tests_pass",
    "pattern_match",
]

# At least one of these regexes must match the captured diff.
expected_patterns = [
    "fn new_thing\\(",
]

# Mock-mode response — exactly what the wiremock will return as the
# Ollama assistant content. Must be a valid unified diff against the
# workspace/ fixture so the worker's apply_patch can take it.
[mock_response]
content = """
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,3 @@
-old line
+new line
"""
```

Verify a new case before committing:

```bash
# 1. The case loads cleanly:
cargo run -q --bin newt-eval -- list-cases

# 2. The mock diff would actually apply to the fixture
#    (independent of the worker — sanity check):
cp -r newt-eval/cases/006-your-case/workspace /tmp/v
cd /tmp/v && git init -q && git add -A && git commit -q -m b
git apply --check /path/to/your.diff && echo OK

# 3. End-to-end with the worker:
cargo test -p newt-eval --test mock_e2e
```

## Test budget

The mock e2e test runs all bundled cases in well under 30 seconds in CI
once the worker binary is built. Each case spawns a worker subprocess,
drives ACP, runs evaluators (including `cargo check` and a quick
`cargo test`), then tears the subprocess down. Per-case timeout is 60s
so a hung live model can't stall CI forever.
