# Yardstick prompts — the 2026-07-06 incident sessions, verbatim

The exact operator prompts from the stall sessions
[`next-loop-levers.md`](../next-loop-levers.md) diagnoses. §5 of that doc
prescribes rerunning these unchanged as the before/after measure for each
lever. Collected verbatim from `~/.newt/conversations.db`.

Incident-night conditions (note deltas on any rerun): newt 0.7.1
(feature-off build), ornith:35b on dgx1, `[tui] max_tool_rounds = 25`,
`summarizer.toml` parked at `.backup` (mid-loop summarizer silently on the
session backend), scratchpad/plan tools unused by the model until nudged.

## Session A — conv `…c21ecf03` (plan mode → implementation)

1. seq 299:
   > Enter planning mode and make me a plan for this issue: https://github.com/hartsock/scrybe/issues/37
2. seq 300:
   > Try now. You should have full ambient access
3. seq 301:
   > Can you start implementation on a new branch please?

   Outcome: 25-round cap; 4 tool-name-as-command hallucinations
   (`phantom_reaches`); 777s wall clock, 462s of it one
   `request_permissions` prompt.

## Session B — conv `…456f38c2` (the plan request)

1. seq 304:
   > Come up with an plan to fix this issue for me: https://github.com/Gilamonster-Foundation/newt-agent/issues/969

   Outcome: 25-round cap, 30 events (incl. 3 off-script `edit_file`
   successes), no plan ledger → no grace, no salvage; assistant output was
   only the 336-char cap banner.
2. seq 305:
   > continue

   Outcome: created a 4-step plan ledger (2× `update_plan`), re-fetched
   the same issue page, ended on a dangling "Let me look at…" narration
   after the single narration nudge was spent.

## Scoring a rerun

Same prompts, same model, one lever changed at a time. Record: rounds
used vs cap, hallucination count, compactions fired (and on which
backend), plan ledger created by round N, grace granted y/n, cap-exit
salvage non-empty y/n, dangling-narration ending y/n, wall clock. A
lever "moves the grade" per house rules only on an n≥5 sweep
(`/ab-gate`), not a single anecdote.
