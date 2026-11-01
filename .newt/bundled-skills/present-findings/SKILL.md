---
name: present-findings
description: Present gathered findings as a rendered report instead of a one-line status. When the task is to gather or summarize (a status roll-up, a triage sweep, a morning briefing), call render_report the moment you have what was asked, and mark a failed data source as a degraded section rather than ending on "X is broken".
when_to_use: When the task is to gather, survey or summarize and the deliverable is information (a status roll-up, a triage sweep, a morning briefing, an audit); when one of several data sources failed and you are tempted to report only the failure; when you are about to answer a gather task with a single sentence.
version: 1.0.0
license: Apache-2.0
caveats:
  exec: { only: [] }
  fs_read: all
  net: { only: [] }
  max_calls: unlimited
---

# Present findings — don't just report a blocker

This was a standing rule of the default system prompt (`habit-present-findings`).
It is a skill now: the prompt stays short, and the rule loads when the task is
the kind it governs.

When the task is to gather or summarize (a status roll-up, a triage sweep, a
morning briefing), your deliverable is a rendered report, not a one-line status.
The moment you have what was asked, call `render_report` to present it.

A failed data source is one degraded section, not a dead end. Render the rest
and mark the failed part `degraded` (or `error`), so the human sees the partial
result plus exactly what is missing. Ending such a task with only "X is broken"
leaves the work you already did invisible.

## In practice

1. Collect from every source you were asked about, even after one fails.
2. Build one report with a section per source.
3. For a source that failed, keep its section: say what you tried, mark it
   `degraded` or `error`, and name what is missing.
4. Call `render_report` once with the whole result; then add one or two
   sentences of your own only if something needs the human's decision.
