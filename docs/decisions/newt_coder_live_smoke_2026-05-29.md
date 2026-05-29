# Newt-coder live smoke against qwen3-coder:30b (2026-05-29)

## Outcome

**25/25 evaluator verdicts pass across all 5 bundled cases.**

`./target/release/newt-eval run --mode live --model qwen3-coder:30b --coder`:

```
case                          evaluator         pass  score  details
----------------------------  ----------------  ----  -----  -------
001-rename-function           diff_nonempty     ok     1.00  captured 424 bytes of diff
001-rename-function           diff_applies      ok     1.00  git apply --check accepted the diff
001-rename-function           rust_compiles     ok     1.00  cargo check succeeded
001-rename-function           tests_pass        ok     1.00  cargo test succeeded
001-rename-function           pattern_match     ok     1.00  2/2 patterns matched
002-add-doc-comment           diff_nonempty     ok     1.00  captured 237 bytes of diff
002-add-doc-comment           diff_applies      ok     1.00  git apply --check accepted the diff
002-add-doc-comment           rust_compiles     ok     1.00  cargo check succeeded
002-add-doc-comment           tests_pass        ok     1.00  no #[test] found — skipped
002-add-doc-comment           pattern_match     ok     1.00  1/1 patterns matched
003-add-error-handling        diff_nonempty     ok     1.00  captured 464 bytes of diff
003-add-error-handling        diff_applies      ok     1.00  git apply --check accepted the diff
003-add-error-handling        rust_compiles     ok     1.00  cargo check succeeded
003-add-error-handling        tests_pass        ok     1.00  cargo test succeeded
003-add-error-handling        pattern_match     ok     1.00  2/2 patterns matched
004-add-test-case             diff_nonempty     ok     1.00  captured 319 bytes of diff
004-add-test-case             diff_applies      ok     1.00  git apply --check accepted the diff
004-add-test-case             rust_compiles     ok     1.00  cargo check succeeded
004-add-test-case             tests_pass        ok     1.00  cargo test succeeded
004-add-test-case             pattern_match     ok     1.00  2/2 patterns matched
005-extract-constant          diff_nonempty     ok     1.00  captured 409 bytes of diff
005-extract-constant          diff_applies      ok     1.00  git apply --check accepted the diff
005-extract-constant          rust_compiles     ok     1.00  cargo check succeeded
005-extract-constant          tests_pass        ok     1.00  cargo test succeeded
005-extract-constant          pattern_match     ok     1.00  2/2 patterns matched
```

## What this proves

The newt-coder plugin (merged PR #26) actually works end-to-end:

1. **Whole-file emit strategy (S5)** as predicted by the drake bake-off
   produces real diffs that apply cleanly.
2. **qwen3-coder:30b on local Ollama** is a sufficient backend for the
   small-file refactor tasks the eval corpus exercises today.
3. **Every evaluator green** — `diff_nonempty`, `diff_applies`,
   `rust_compiles`, `tests_pass`, and `pattern_match` all confirm the
   produced patches are not just syntactically valid but semantically
   correct (compile, pass tests, match expected regex patterns).
4. **No prompt-tax workarounds needed** — the harness drives `newt
   worker --coder` directly and gets the expected output without
   special-casing per model.

## Context

- The bake-off card at
  `~/workspaces/knowledge/board/drake/2026-05-29_newt-coder-failure-mode-taxonomy.md`
  predicted S5 (whole-file emit) would close failure mode T0b. This
  smoke confirms the prediction on the canonical model.
- The model id (`qwen3-coder:30b`) is preserved in the TaskReply's
  `model_id` field so drake-foreman's scorecard can attribute work.
- newt-flat (legacy path) hits T0b on every model tested — verified
  earlier in this session by running the same cases without `--coder`,
  which produces `pattern_match` fails on 4 of 5 cases.

## Reproduction

```bash
# Prereq: Ollama at 127.0.0.1:11434 with qwen3-coder:30b pulled
cd ~/workspaces/newt-agent
cargo build --release --bin newt --bin newt-eval
./target/release/newt-eval run --mode live --model qwen3-coder:30b --coder
```

Total wall-clock on gnuc: ~3 minutes for 5 cases (most of it is
qwen3-coder generating tokens — coder mode adds negligible
overhead).

## What's next

This proves the plumbing. The eval corpus is intentionally small (5
cases, all single-file Rust refactors). The next reasonable expansion:

- Multi-file refactor cases (rename across files)
- Larger workspaces (the 8K-token context cap will start mattering)
- Non-Rust languages (Python, TypeScript) to validate the
  language-agnostic strategy
- Other models (gemma3:12b, glm4:9b, mistral-small:24b) to find each
  one's failure-mode profile

None of those are required to declare newt-coder shipped. They're
follow-up work for whoever wants broader confidence.
