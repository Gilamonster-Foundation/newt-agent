# newt-on-newt: roadmap step 27.2, measured

Evidence for **#2212**. Phase 27 (27.3, 27.5).

`newt solve --confined` was pointed at this repository and given roadmap step
27.2 as a prose spec with the answer withheld. This directory holds the trace of
what it did, so the run survives independently of a temp directory and of
anyone's session.

## Read this before reading the trace

**The trace contradicts itself, and that is the point.** The final record says
`"outcome": "completed"` while the `solve_result` record says
`"end_reason": "Some(RoundCap)"` and `"status": "completed"` — a run that hit its
round cap is reported as a successful completion.

That is **faithfully what the harness emitted**. It is not a capture error, not a
transcription slip, and not something to normalise away. It is the defect #2212
exists to investigate. A trace tidied into internal consistency would document a
defect that does not exist while hiding the one that does.

## What the run produced

It wrote a **byte-identical-to-reference** implementation, in three files:

| file | change |
|---|---|
| `newt-core/src/agentic/git_tool.rs` | +6 |
| `newt-git/src/lib.rs` | +1 / −1 |
| `newt-git/src/lib_tests/git_scope.rs` | one new test |

The three `edit_file` calls are trajectory entries **26, 27 and 28 of 128**
(0-based indices 25–27). It then made **100 further calls** and stopped at the
round cap.

**It never ran the test the task required.**

## Headline numbers

| | |
|---|---|
| tool calls | **128** (write calls: 3) |
| calls after the last edit | **100** |
| `run_command` | 36 |
| `text_search` | 31 |
| `read_file` | 28 |
| failed calls, all tools | **17** — `run_command` 11, `artifact_read` 5, `lifecycle` 1 |
| slowest call | **60,008 ms**, a failed `run_command` — ~9% of total wall time in one timeout |
| hallucinations | 2 |
| tokens | 85,142 (22,287 generated) |
| wall time | 649.28 s |
| rounds | 45 `chat_completion_finish` records against `max_rounds: 40` |
| end_reason / outcome | `Some(RoundCap)` / `completed` — see above |

## How correctness was established

**Not by trusting the run.** The output was diffed against a reference extracted
independently and earlier, from a different and much stronger model's work. The
run's own reporting cannot establish its correctness — as the `outcome` field
demonstrates.

## Files

- **`newt-on-newt-27.2-trajectory.jsonl`** — 47 records: one `solve_result`
  carrying the 128-entry trajectory, 45 `chat_completion_finish` records
  (rounds 0–44), and the final summary. Each trajectory entry is
  `{tool, args_digest, duration_ms, ok}`. `args_digest` is a parameter-name list
  plus a BLAKE3 hash (`"new_string old_string path b3:6bf490b2e3465278"`), so it
  records argument *shape*, never argument contents.
- **`task27_2.txt`** — the exact instruction the run was given, verbatim,
  included so the scenario can be replayed.

## Redactions

Four environment fields were replaced before committing, because this repository
is public. Nothing else was altered: all 47 records, all 128 trajectory entries,
and every metric are as emitted.

| record | field | replaced with |
|---|---|---|
| 1 (`solve_result`) | `endpoint` | `<redacted: an OpenAI-compatible endpoint>` |
| 1 | `cwd` | a repo-relative path |
| 1 | `task_file` | the committed repo-relative path of `task27_2.txt` |
| 47 (summary) | `backend.name` | `<redacted>` |

The endpoint was a private router; its address and name are deliberately absent.

## For a future eval fixture

Phase 27 calls for a `newt-eval` BAT/UAT case. This directory is a tracked,
stable path a fixture can point at. Two cautions for anything asserting on the
file: do not assert on the four redacted values, and **do not normalise the
`end_reason`/`outcome` disagreement** — an assertion that the two disagree is a
legitimate regression test for #2212.
