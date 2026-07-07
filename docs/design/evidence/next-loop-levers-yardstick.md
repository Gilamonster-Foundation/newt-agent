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

## Reproducing on another system

The goal is to reproduce the *baseline failure* first, then flip one
lever at a time. Everything below is what a clean box needs.

**1. Pin the code.** The incident binary is `v0.7.1` (tag `12eaac5`;
`305d56d` also reproduces — the delta is an unrelated release fix).
Build it exactly as the incident did — **without** the `embedded`
feature (that gap is part of the baseline):

```bash
git clone https://github.com/Gilamonster-Foundation/newt-agent
cd newt-agent && git checkout v0.7.1
just install ~/bin        # plain install = feature-off, as on the incident box
```

**2. Serve the model.** An Ollama host serving `ornith:35b` (the
built-in model card ships in-repo — `newt dgx card show` / #854; any
box Ollama runs on works, GPU strongly recommended for a 35B). The
failure *class* reproduces on any ~30B thinking model that emits a
`thinking` field, but exact-comparison runs want ornith. The incident
host also served other agents concurrently — an idle server softens
the summarizer-contention leg (§2.4 of the doc); note it when scoring.

**3. Reproduce the incident config.** Minimal `~/.newt/config.toml`
(the load-bearing knobs are `max_tool_rounds = 25` and the trim
threshold; everything else shown for fidelity):

```toml
[[backends]]
name = "primary"
endpoint = "http://<ollama-host>:11434"
model = "ornith:35b"
kind = "ollama"

[tui]
max_tool_rounds = 25          # the incident pin (shipped default is 40)
mid_loop_trim_threshold = 40  # clamps to 22 at runtime (= max_tool_rounds - 3)
inference_timeout_secs = 120
keep_alive = "5m"

[tui.permissions]
preset = "workspace_dev"
prompt = true
```

And **no** `~/.newt/summarizer.toml` — its absence is the incident
condition (mid-loop compaction silently inherits the session backend).
A fresh box also has an unprobed `model-capabilities.json`; that
matches the incident (ornith's conformance was never probed there
either). To remove the human-latency confound from Session A, grant
session-level full access when prompted instead of leaving the
permission dialog waiting (the 462s of seq 301 was a human, not the
loop).

**4. Run the tasks.** Launch `newt` inside the cloned `newt-agent`
workspace and paste the prompts verbatim. Session B is fully
reproducible (issue #969 is public in this repo). Session A's issue
(`hartsock/scrybe#37`) is in a **private** repo — substitute any small,
concrete issue URL the box can fetch and hold it constant across runs.

**5. Observe.** The baseline failure signature:

- Turn ends in the cap banner: `(reached the tool-call limit of 25
  rounds … the final tools-disabled summary described future tool
  actions instead of final state …)` — with **no** "Progress captured"
  block (empty salvage).
- A `continue` follow-up re-fetches URLs/files from the prior turn and
  ends on a dangling "Let me …" narration.

Where to look, per run:

```bash
# rounds/events, ending text, per-turn tokens
sqlite3 ~/.newt/conversations.db \
  "SELECT seq, json_array_length(events), tokens_in, tokens_out,
          substr(assistant,-300) FROM turns ORDER BY seq DESC LIMIT 3;"
# hallucination count per turn
tail -5 ~/.newt/usage.jsonl
# summarizer resolution + compaction events (stderr)
RUST_LOG=info newt 2> /tmp/newt-run.log   # grep for 'summariz' after the run
```

## Scoring a rerun

Same prompts, same model, one lever changed at a time. Record: rounds
used vs cap, hallucination count, compactions fired (and on which
backend), plan ledger created by round N, grace granted y/n, cap-exit
salvage non-empty y/n, dangling-narration ending y/n, wall clock. A
lever "moves the grade" per house rules only on an n≥5 sweep
(`/ab-gate`), not a single anecdote.
