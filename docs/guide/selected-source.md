# Keep a selected source attached to the objective

When a task selects one source to change, include its full repository-relative
path, search scope and observed evidence in the existing `update_plan` call:

```json
{
  "target": {
    "path": "engine/parsing/document.py",
    "scope": "tracked implementation files across the repository",
    "evidence": "Repository-wide inventory ranked this file first.",
    "revision": "Initial selection for the requested refactor."
  },
  "plan": [
    {"step": "Extract a cohesive component from engine/parsing/document.py"},
    {"step": "Check the affected package"}
  ]
}
```

The harness pins the selection independently of plan-step progress. Ordinary
plan updates omit `target` and retain it. Approval, continuation, conversation
restore and context compaction retain the full path. A missing file under a
different crate is a reason to rediscover the recorded path, not to silently
choose another source.

To revise the selection, send another `target` with the new evidence and an
explicit revision reason. The new selection names the previous content identity.
Both snapshots remain in the existing plan-revision artifact history. A failed
artifact append retains the old selection; sessions without artifact recording
cannot accept a new selection. `/new` clears the session plan and pin.

Before model requests, including the final summary, the harness compares
committed and working changes against the HEAD captured at initial selection.
It also considers untracked files. If changes exist but the selected source is
absent, the model receives **“work landed outside the selected source”** before
finalizing. Rename origins count as changes to the source.

This is advisory reconciliation, not a write restriction or proof of completion.
It introduces no filesystem grants. If the existing read authority cannot inspect
Git metadata, the comparison is unverified rather than successful. Comparison
uses in-process index parsing, verified object reads and raw-byte hashing, never
Git conversion programs. Filters, attributes, EOL conversion and unsupported
repository states likewise produce an unverified result. The HEAD
comparison can include pre-existing uncommitted changes; it does not attribute
authorship. The harness retains the model's selection and evidence, but does not
independently prove that the selection satisfies a natural-language superlative.
