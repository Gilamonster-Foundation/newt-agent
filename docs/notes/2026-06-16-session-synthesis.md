# Session synthesis — 2026-06-16

A long working session that shipped the loadout audit/provider surface, landed two
adjacent-agent PRs, validated the reasoning spinner + tool-recovery live on a real
model, and — most importantly — figured out three new architecture threads:
the **crew loadout**, the **captured-shell OCAP model**, and the **agent-mesh
availability pool**. This note is the index; the two design notes carry the detail.

---

## 1. What shipped (merged to `main`)

| PR | What |
|---|---|
| #397 | **Loadout Slice 2** — `provider` axis selects a named `[backends]` entry; unified `resolve_backend_choice` precedence |
| #403 | **Disk loadouts** — drop `~/.newt/loadouts/*.toml` (mirrors the disk-bundle loader) |
| #404 | **`/loadout show`** — declared axes vs what resolved, the in-session audit view |
| #400 | **`/config`** — dump the resolved config with secrets redacted, for audit |
| #402 | **Reasoning spinner + `/backend` + `/thinking`** (other agent's; rebased + unified) — `ThinkFilter::feed_split` surfaces reasoning live; the backend axis unified with Slice 2's `NEWT_PROVIDER` |
| #405 | **Tool-rejection recovery** (other agent's) — models that 400 on a `tools` field (deepseek-r1) drop tools and continue instead of dying |
| #395 / #396 / #406 | **Design specs** — paged→**progressive-disclosure compaction**, the context scheduler, and the reconciliation corrections |

The unified backend precedence (from #397 + #402) is now:

```
NEWT_PROVIDER (named [backends] entry)  ▸  NEWT_BACKEND (/backend kind toggle)  ▸  prefer-openai  ▸  Ollama/DGX
```

`/config` + `/loadout` are the audit surfaces; `/backend` + `/thinking` are the live
switches; `/loadout <name>` (full mid-session switch) is the remaining loadout slice.

## 2. Live validation (on `deepseek-r1:1.5b`, gnuc)

Drove a real reasoning model under a PTY to validate two freshly-landed features:

- ✅ **Reasoning spinner** — animated (436 redraws), reasoning shown as dim scrollback,
  erased cleanly when the answer began, no `<think>` leak, correct answer.
- ✅ **Graceful tool-degradation** — `does not support tools — tools disabled for this
  session` fired, the session continued.
- ⚠️ **One cosmetic seam** — the pre-existing grey `▸ thinking…` placeholder isn't cleared
  before a notice prints, so they run together on one line. Functional, minor.

## 3. The three new architecture threads

### A. Crew loadout — [`design/crew-loadout.md`](../design/crew-loadout.md)
The role-specialized sibling of the diversity panel: a **named ensemble of role-loadouts
+ a control program**. The three-model coding pattern (planner / navigator / triage) is a
crew. Includes the **heterogeneous inference pool** (place roles across machines), the
**model-residency scheduler** (the gap #396's kv-warden didn't cover), and the
**agent-mesh availability** layer (the pool *breathes* — presence-based membership so
intermittent peers are normal-mode).

### B. Captured-shell OCAP + policy assistant — [`design/captured-shell-ocap.md`](../design/captured-shell-ocap.md)
A persistent, OS-isolated, interpreter-mediated shell (a reference-monitor sandbox) that
holds session state — solving the `pa login` credential workflow without ambient host
authority — plus a **policy-authoring assistant** (carve-outs are beyond hand-authoring).
**Adversarially threat-modeled this session** (75-agent panel): verdict
`unsound-needs-rework` — the authority *algebra* is sound, the *enforcement* is unbuilt.

### C. The Centaur-swarm vision (the framing)
The north star both threads serve: a human **pilots** a swarm of diverse models.
**Diversity** (decorrelated voices) breaks LLM groupthink; **containment** (attenuation +
curated-context + verify-gate, vs the Confused Deputy) is what makes diverse/foreign
voices safe. Diversity and containment are the same coin. The crew and the captured shell
are the same machinery used two ways: division-of-labor and credential-safety. The human
moves from *author* of policy to *approver* of machine-proposed policy.

## 4. Load-bearing decisions & findings

- **Naming:** "progressive-disclosure" over "paging" (the innovation, not the metaphor);
  "**crew loadout**" for the role-specialized ensemble.
- **Scheduler crate-home:** `newt-scheduler` (default build), **not** `newt-mesh` (which
  path-deps `agent-mesh` and cargo validates eagerly even behind a feature).
- **`per-model-strategy` is a profile knob** (`context_strategy`), not a new tier or a
  second `[[profiles]]` table.
- **OCAP threat model:** redaction is **not** the security boundary — the egress proxy +
  keeping the token **out of the box** is. No live `pa` credential until OS-isolation (B1)
  is a fail-closed precondition.
- **`pa login` credentials** are short-lived scoped **files** → already capability-by-
  reference → the OCAP-clean primitive. The confined shell runs `do_not_inherit_env(true)`,
  so ambient env never reaches it (that block is OCAP working, not a bug).

## 5. The real inference pool (measured)

| Backend | Reach | Notable models |
|---|---|---|
| **gnuc** (`geforcenuc`) | `localhost:11434` / `REDACTED-HOST` | `qwen3-coder:30b`, `qwen2.5-coder:{3b,7b,14b,32b}`, `codestral:22b`, `mistral-small:24b`, `nemotron-3-nano`, `deepseek-r1:1.5b` |
| **DGX** | `REDACTED-HOST` (Traefik) / cluster `ollama-proxy` | `devstral-small-2:24b`, `qwen3-coder:30b`, `qwen2.5-coder:32b`, `codellama:70b`, `deepseek-r1:{32b,70b}`, `nemotron:70b`, `qwen3.6:35b` |
| **Windows boxes** | house LAN, **intermittent** (serve other functions) | (varies; join the pool when free) |

Crew role placement (fieldable today): **planner** `qwen3-coder:30b` (either, failover-able),
**navigator** `devstral-small-2:24b` (**DGX-only**), **triage** `qwen2.5-coder:3b` (gnuc,
always-resident). Happy path → **zero model swaps**.

## 6. Open threads / next steps

- **Crew MVP (#85):** the empirical "boring two-pass machine" across gnuc+DGX — expose the
  real failover/latency problems on real hardware before building the scheduler crate.
- **Mesh-availability grounding** (`wf wdjb57cxu`): confirming reuse-vs-net-new against the
  real agent-mesh / newt-mesh code; folds into `crew-loadout.md`.
- **OCAP rework (#84):** the three MUSTs — fail-closed B1, the in-process monitor holes,
  the real human spine — gate any live-credential work.
- **Loadout layer (#82):** mid-session `/loadout <name>` switch, address-string sugar,
  `model@variant` (catalog #387).
- **Cosmetic:** clear the `▸ thinking…` placeholder before a notice prints.
