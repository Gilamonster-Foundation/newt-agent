# Observed facts in final reports

Final task reports begin with an **Observed** section assembled by the harness.
The model's explanation follows it. This includes tool-round-cap handoffs.

The report shows changed files relative to the first objective/adoption snapshot,
with before and after LF counts (`wc -l` semantics), the last observed Cargo check
or test per exact command and directory, bounded child-reported result lines,
and governed push/PR creation receipts. A committed change remains visible even
when the working tree is clean. Bare filenames such as `mod.rs` are marked
ambiguous when they could refer to multiple observed files.

These are historical observations. A previous successful check does not certify
subsequent edits, and an observed PR creation URL does not certify its current
remote state. A pipeline's recorded exit is the whole command's exit, not proof
of an individual child's exit. Missing or inaccessible evidence is labelled
unavailable; a dry-run push is not a publication receipt.

The session retains facts across continuations of the same objective. A new
objective or explicit worktree lift resets them. Retention is bounded to four
roots, sixteen exact check scopes and sixteen publication observations per root.
File snapshots consider at most 4,096 Git-tracked/nonignored files, two MiB per
file and 32 MiB total. Unsafe, binary or oversized files have unavailable line
counts. Reports show at most 64 changed/unavailable file rows and eight bounded
result lines per check, and disclose omitted evidence. This in-memory retention
does not survive a process restart.

The report record uses the existing content-addressable canonical codec; its
content ID covers the objective/root, baseline/current snapshot identities and
observations. This is the first harness-owned pinned report record. Model text
cannot create a governed receipt or replace a stored observation.

Stage 1 leaves the model's explanatory prose intact, including any conflicting
numbers or conclusions. Clause-level correction of those claims is a later stage.
