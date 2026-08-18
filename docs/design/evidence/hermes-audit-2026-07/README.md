# Evidence — hermes-agent architectural audit (2026-07-06)

Raw survey reports behind [`../../hermes-audit-2026-07.md`](../../hermes-audit-2026-07.md).
Follow-up to the 2026-06-10 study in [`../hermes-study/`](../hermes-study/).

Subject: [hermes-agent](https://github.com/NousResearch/hermes-agent)
(NousResearch, MIT) @ `origin/main` commit `830165473` (2026-07-06). File
citations in these reports use the form `hermes-agent@830165473:path/file.py:line`;
line numbers drift fast in that repo (some files grew >1,000 lines in the
month before this audit), so treat them as anchors for the *mechanism*, not
exact coordinates.

Ten parallel survey agents, one report each:

| Report | Scope |
|---|---|
| `report-agent-loop.md` | turn engine, retry/failover, provider transports, prompt-cache discipline, done-detection, verify-on-stop |
| `report-sessions-delta.md` | session/state layer — verification of the June study's claims + what changed (schema v11→v19, routing-table move, in-place compaction) |
| `report-memory-learning.md` | curated memory files, nudge cadence, background review fork, FTS recall, Honcho plugin |
| `report-skills.md` | skill format/injection, autonomous creation/self-improvement loop, curator, guards |
| `report-tools-rpc.md` | tool registry/toolsets, 3-layer output budgets, execute_code script-RPC, MCP, tool-search deferral |
| `report-subagents.md` | delegate_task isolation/budgets/supervision, async delivery, batch/mini-SWE runners |
| `report-gateway-deploy.md` | platform gateway, terminal-backend seam (local/Docker/SSH/Modal/Daytona), cron, ops story |
| `report-context-compression.md` | runtime compressor delta, temporal anchoring, reasoning-echo matrix, output bounding |
| `report-engineering-shape.md` | repo shape, test/CI reality, velocity decomposition, docs/plugin/config surface |
| `report-settled-verdicts.md` | decision ledger from the June study — verdicts/adoptions/rejections that gate this audit |

Method note: reports were produced against a pinned read-only worktree of the
upstream commit (not the operator's month-stale local branch), by agents
instructed to distinguish wired-in behavior from README claims and to end with
explicit "Candidate learnings for newt" / "Avoid replicating" sections. The
synthesis doc then passed a three-lens adversarial critique before
finalization — the critiques are preserved here too, because they materially
changed the verdicts (see the doc's Critique log):

| Critique | Lens |
|---|---|

**Retired 2026-08-18.** The three `critique-*.md` review artifacts
(factual, fit, completeness) were removed. Each reviewed a draft of the
audit, and the audit was then revised against them, so their content is
in the document they were reviewing. They are in git history before this
commit if the review itself is ever the question.
