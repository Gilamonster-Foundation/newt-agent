# Observed facts in final reports

Final task reports begin with an **Observed** section assembled by the harness.
The model's explanation follows it. This includes tool-round-cap handoffs.
Anthropic answer deltas are buffered until finalization so the report and
explanation print once; SSE consumption and idle-timeout handling continue.

The Observed section is operator output, not assistant-authored speech. New
conversation rows store only the model explanation; legacy mixed rows are
projected the same way when replayed, without rewriting their history. The
existing turn-outcome artifact retains the displayed report separately (with an
explicit excerpt notice if its body limit is reached). Current facts reach all
four provider APIs in a labelled harness system note. Paths, commands and result
excerpts are framed as data; the note never substitutes for an operator request.
Smart navigation preserves its existing content-addressed harness pin provenance.
Restored plans are labelled agent-maintained advisory context rather than system
policy. Bare continuations retain the objective after normal finishes as well as
round-cap pauses.

The report shows changed files relative to the first objective/adoption snapshot,
with before and after LF counts (`wc -l` semantics), the last observed Cargo check
or test per exact command and directory, bounded child-reported result lines,
and governed push/PR creation receipts. A committed change remains visible even
when the working tree is clean. A path dropping out of Git's listing is reread
through the authorized reader: an ignored but present file retains its measured
count, and only verified absence is labelled `0 (absent)`. A newly discovered
path has an unverified baseline unless absence was actually observed there;
current contents cannot establish a historical count. Bare filenames such as `mod.rs` are marked
ambiguous when they could refer to multiple observed files.

These are historical observations. A previous successful check does not certify
subsequent edits, and an observed PR creation URL does not certify its current
remote state. A pipeline's recorded exit is the whole command's exit, not proof
of an individual child's exit. Missing or inaccessible evidence is labelled
unavailable; a dry-run push is not a publication receipt.

The session retains facts across continuations of the same objective. A new
objective or explicit worktree lift resets them. Retention is bounded to four
roots, sixteen exact check scopes and sixteen publication observations per root.
File snapshots consider at most 4,096 paths (Git-tracked/nonignored files plus
formerly observed paths omitted from the current listing), two MiB per
file and 32 MiB total. Unsafe, binary or oversized files have unavailable line
counts. Reports show at most 64 changed/unavailable file rows and eight bounded
result lines per check, and disclose omitted evidence. This in-memory retention
does not survive a process restart.

The report record uses the existing content-addressable canonical codec; its
content ID covers the objective/root, baseline/current snapshot identities and
observations. This is the first harness-owned pinned report record. Model text
cannot create a governed receipt or replace a stored observation.

Only complete copies matching the rendered report structure outside code fences
are removed from model prose. Partial or ambiguous report-like text is preserved.
Stage 1 otherwise leaves the model's explanation intact, including any conflicting
numbers or conclusions. Clause-level correction of those claims is a later stage.
