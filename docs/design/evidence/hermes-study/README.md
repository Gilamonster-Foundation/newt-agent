# Evidence — hermes-agent ↔ newt-agent context/memory study (2026-06-10)

Raw, unedited outputs of the multi-agent study behind
[`docs/design/context-memory-hermes-learnings.md`](../../context-memory-hermes-learnings.md).
Committed so the plan's claims are auditable without rerunning the study.

**Provenance.** An 8-agent orchestrated workflow (run id `wf_57b8414c-cdb`):
four parallel deep-readers, one synthesis agent, then a three-lens
adversarial verification panel instructed to *refute* the synthesis against
both codebases. Studied trees: newt-agent @ `6b7d780`, hermes-agent
(NousResearch, MIT) @ the local checkout of 2026-06-10.

| File | What it is |
|---|---|
| `report-hermes-context.md` | Reader 1: hermes context-window engine (context_engine, context_compressor, references, token accounting) |
| `report-hermes-memory.md` | Reader 2: hermes memory/curation (memory_manager, curator, memory_tool, nudges, Honcho) |
| `report-hermes-sessions.md` | Reader 3: hermes session management (gateway sessions, FTS5 search, resume/handoff, hermes_state) |
| `report-newt-current.md` | Reader 4: newt's current memory/conversation/context systems + limitations |
| `verdict-hermes-claims.md` | Verifier A: every hermes-side claim confirmed/refuted with file:line (42/50 confirmed; found 4 missed mechanisms) |
| `verdict-newt-claims.md` | Verifier B: every newt-side claim checked (found 5 proposals that already exist in newt; corrected the 9.7 sequencing premise) |
| `verdict-fit.md` | Verifier C: constraint/fit review (dep argument, PR sizing, phase numbering, decision-doc conflicts) |

**Reading order for an audit:** the verdicts first (they index the draft's
errors), then the draft, then the reports for the underlying evidence. The
design doc is the draft **with every verdict correction applied**; where the
doc and the draft disagree, the doc is right on purpose.

**Retired 2026-08-18.** `synthesis-draft-plan.md` was removed. This README
already recorded it as superseded by the corrected design doc, and nothing
cited it. `upstream-offers-to-hermes.md` went with it: it was advice addressed
to a different project. Both are in git history before this commit.
