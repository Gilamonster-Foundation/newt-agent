# Sweep Analysis: 2026-07-20-ornith-crew-ladder

- **Sweep dir:** `scripts/eval/results/sweeps/2026-07-20-ornith-crew-ladder`
- **Git SHA:** `8844352` (main + the #1321 config bridge; merged as `35a2536`)
- **Graded rows:** 20
- **Status:** DONE=true (sweep complete; errors.log empty)
- **Context:** the build-farm handoff §E ratchet run (n≥5, the #803 lesson).
  Backend: Ornith-1.0-35B-NVFP4 on vLLM (`kind = "openai"`), pinned per-cell via
  the #1321 project-config bridge — the first crew sweep after #1320 exposed the
  silently-inert `$NEWT_CONFIG` export (any earlier crew sweep on an affected
  binary is suspect; this one is provenance-stamped and pin-verified).

## Summary

**The T0–T3 ladder does not reach Ornith crew mode's competence boundary: 20/20
PASS** (pooled Wilson 95% [84%, 100%]; every cell 5/5, [57%, 100%]). Every row
landed a real crew branch with clean anti-gaming diagnostics
(`touched_src_lib=yes`, `edited_own_test=no`, `plan_rc=0`), mean trial duration
12s. No infra rows, no gameable results.

**Honest caveat — the rungs never engaged decomposition:** every trial planned
`leaves=1`. T0–T3 as bundled are one-leaf tasks for a model of this strength, so
the known crew failure mode (multi-leaf mis-grounding → orphan vacuum files, the
#672/E diagnosis) was never exercised. The boundary hunt continues in the
companion sweep `2026-07-20-ornith-crew-hard` (008/010/011/013/014 — the rungs
where the pr802 baseline measured crew at 14/25 pooled on 010).

**OCAP note (the handoff card's load-bearing check):** ratchet/sweep set no
access override — these crew leaves ran under the **confined default engine**
with the `session ⊓ crew_clamp` dispatch bound. This sweep is therefore the
"useful-AND-OCAP" arm: 100% behavioral pass **under confinement**.

## Per-cell results

| task | mode | model | pass | fail | n | pass-rate | Wilson 95% |
|---|---|---|---|---|---|---|---|
| T0-fix-add | crew | Ornith-1.0-35B-NVFP4 | 5 | 0 | 5 | 100% | [57%, 100%] |
| T1-parse-port | crew | Ornith-1.0-35B-NVFP4 | 5 | 0 | 5 | 100% | [57%, 100%] |
| T2-humanize-duration | crew | Ornith-1.0-35B-NVFP4 | 5 | 0 | 5 | 100% | [57%, 100%] |
| T3-format-temperature | crew | Ornith-1.0-35B-NVFP4 | 5 | 0 | 5 | 100% | [57%, 100%] |

## Comparisons

- **vs `2026-07-01-pr802-baseline`:** T2/crew there pooled 19/25 across five
  ollama-served models (per-model cells from 1/5 to 5/5). Ornith T2/crew here is
  5/5. Cross-sweep model/backend differences make this directional, not causal.
- **Single mode absent by design:** the ACP worker's env-shim speaks ollama
  `/api/chat`; Ornith serves only vLLM/openai. Running single would only trip
  the dead-endpoint canary. Recorded as a gap: either the worker learns openai
  or the router serves Ornith.

## Security invariant

Model identities only; endpoints/hosts live in the operator's local
`~/.newt/eval-sweeps/` template (RATCHET.md invariant). The TSV's `dir=` values
are throwaway `/var/tmp` cells.
