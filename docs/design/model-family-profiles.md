# Model-family profiles — a tuned harness per model family

**Status:** design note · **Depends on:** the cross-family forensic (below) ·
**Composes with:** Phase 9.8 `AgenticConfig`, Phase 20 self-tuning ·
**Roadmap slot:** Step 20.3 (the outer, family-wide layer of Phase 20) ·
**Scope guardrail:** [`docs/decisions/plain_scroller_tui.md`](../decisions/plain_scroller_tui.md)

## Abstract

The harness is compiler/linter/IDE tooling aimed at a model, and different
model families need different tooling. A **profile** is a named, toggleable
bundle of harness behaviors — window budget, compression mode, tool-round cap,
verify strictness, decomposition, disclosure budget, re-ground-after-compression,
prompt/soul shape — tuned to how a model *family* behaves. Newt ships a **profile
suite** (`nemotron`, `qwen-coder`, `default`); by default the **model implies the
profile**, and `--profile <name>` **overrides** that default — letting you set a
**model AND a profile** independently (`newt --model nemotron3:33b --profile
nemotron`). The winning profile values are **discovered by the ground-truth
sweep, not guessed**. This note specifies the profile schema, the selection UX,
the model × profile matrix the override unlocks, where profiles live, a worked
`nemotron` profile, and a crawl/walk/run staging.

Profile names are **provisional** — `nemotron` for lack of a better term — and
the suite is **open-ended**: new profiles join as the sweep finds families that
need distinct tooling. `default` is **today's harness, unchanged** — the safe
baseline.

## Naming — discovered, not declared

A profile is named for its **target** (`nemotron`), not its behavior — on
purpose. Naming it for what it *does* (`self-grounding`, `knowledge-base`,
`ast-support`) presumes the diagnosis before the sweep returns it; `nemotron`
names the model we're tuning *for* and stays honest about the rest. The
discipline: **name the target now, measure what it needs, then name the things it
needs** — names follow measurement, the same ethos as discovered-not-guessed
profile *values*.

The capability vocabulary will emerge from the schema's knobs, because the names
that feel right already map onto them: **knowledge-base** ≈ the FFI manifest (R1,
a KB of the real import surface that survives compression); **self-grounding** ≈
forced re-ground-after-compression (R4); **ast-support / symbol-tree** ≈ the
symbol-level oracle (#74); **python-ide / rust-ide** ≈ the per-language verify
adapters. The destination — not yet, and not a commitment here — is a profile
**composed** from named capabilities (`nemotron = knowledge-base + self-grounding
+ ast-support`), where any model selects the capabilities its failure mode needs.
Until the sweep tells us which knobs Nemotron actually needs, the profile keeps
its target name and the suite stays open.

## Motivation — why per family

Two findings make this concrete and force the design.

[`docs/findings/2026-06-14-cross-family-confabulation.md`](../findings/2026-06-14-cross-family-confabulation.md)
drove the identical task (one Python example per PyO3 crate, a corpus that
overflows the effective window) through the rig across three models. **Both**
nemotron models fail completely (score 0.0), fabricating an import surface from
the *crate* names (`import newt_core`); `qwen3-coder:30b` — same harness, same
corpus, same prompt — **passes** (1.0), finding the real `newt_agent.*` umbrella.
The failure is **family-specific, not structural**: a nemotron-family × harness
interaction, not an inevitability of overflow. Its explicit implication is
"model-family packs — a tuned support profile per family … discovered by
measurement … applied automatically."

[`docs/findings/2026-06-14-fabrication-is-sampling-not-information-loss.md`](../findings/2026-06-14-fabrication-is-sampling-not-information-loss.md)
diffs a **passing** and a **failing** run of the *same* model on the *same*
hardware, with byte-identical inputs (md5-verified). The model **had** the
surface — it read the `pyo3_module.rs` files that literally declare
`module = "newt_agent._newt_agent.core"`, delivered in full — and on the failing
roll overrode it with a crate-name prior. **Compression fired in both runs**
(~24.8k→5.7k pass, ~24.7k→4.3k fail). So fabrication is **post-compression
sampling variance, not information loss**. That reframes the levers: more context
or a louder re-read nudge cannot fix a model that already had and ignored the
answer. The load-bearing remedies are (R1) make the surface a
compression-surviving structured fact (the FFI manifest, #74); (R2) verify-gated
revert-and-retry (#73) to exploit the stochasticity (N attempts → ≈ `p^N`); and
(R5) per-family tuning as a post-compression-recovery knob — "the concrete
content of a newt-agent **for** nemotron profile."

A profile is the unit that bundles those knobs per family. nemotron needs the
manifest, strict verify + revert-retry, and forced re-grounding; qwen-coder
largely needs none of it *here* → a lighter profile. Which knobs and which values
are a measurement result, not an opinion.

## What a profile IS — the schema

A profile is a named bundle of harness-behavior knobs. The table below is the
proposed schema, one row per knob, each mapped to the **real seam** that already
implements (or would implement) it, and flagged **config-driven today** vs
**hardcoded — needs a seam**. A profile *selects which values to apply*; it does
not invent new mechanism.

| Knob | What it controls | Real seam | Status |
|---|---|---|---|
| **window budget** | proven/believed input floor → send budget | `ChatCtx::max_ok_input` / `safe_context` (`newt-core/src/agentic/mod.rs:446,455`) feeding `initial_send_budget` (`mod.rs:146`) | **config-driven today** (via `ModelTuning` / capabilities cache) |
| **tool-round cap** | tool-call iterations before forced final completion | `TurnDriverConfig::max_tool_rounds` (`newt-core/src/agentic/driver.rs:83,122`); per-model via `ModelTuning::max_tool_rounds` | **config-driven today** |
| **compression thresholds** | when to compress | `compression_trigger` (`newt-core/src/agentic/compress.rs:272-318`); `ModelTuning::mid_loop_trim_threshold` / `mid_loop_trim_tokens` | **config-driven today** (thresholds only) |
| **`on_pre_compress` fact-preservation** | what the summarizer *keeps* (R3: preserve symbol/import facts, not prose) | the compression path around `compression_trigger` (`compress.rs:272-318`) | **gap — needs a seam** |
| **verify strictness + revert-retry (N)** | whether/how hard fabricated symbols gate, and how many revert-and-retry attempts | verify oracle `Verdict::Fabricated` (`newt-core/src/symbols.rs:144`, resolve ~220/234); revert-retry is #73 | oracle exists; **strictness + N — needs a seam** |
| **decomposition on/off** | split a task so each subtask's working set fits | no agentic-loop knob; planning is external (`newt-core/src/plan.rs:42`, #334, Workstream C) | **gap — needs a seam** (the loop is decomposition-blind today) |
| **disclosure / memory budget** | how many items the memory index surfaces | `MEMORY_INDEX_BUDGET` (`newt-core/src/memory.rs:763`, hardcoded `12`) | **hardcoded — needs a config knob** |
| **re-ground-after-compression** | force recovery of the evicted surface after a compress | `reread_breadcrumb` (`newt-core/src/agentic/compress.rs:892`) — deterministic, no knob; R4 wants it to carry the *fact* | **gap — needs a seam** |
| **prompt / soul shape** | per-family system-prompt framing | mode framing pattern (`ModeConfig.framing`, `newt-core/src/config.rs:138-151`) is the closest existing shape | **gap for the family axis — scoped, deferred** |

**Where the schema stands today.** Three knobs are already (at least partly)
config-driven — **window budget**, **tool-round cap**, and **compression
thresholds** — the Phase 9.8 `AgenticConfig` surface a profile can set right now.
The remaining knobs each **need a new seam** before a profile can set them:
`on_pre_compress` fact-preservation, verify strictness + retry-`N`,
decomposition, the disclosure/memory budget (today a hardcoded
`MEMORY_INDEX_BUDGET = 12`), the re-ground breadcrumb's form/priority, and the
per-family prompt/soul shape. (Note: the session-bound `memory_source` gate at
`mod.rs:479-487` decides only whether the `memory_fetch` tool is advertised — it
is **not** a profile-settable knob and is out of scope here.) A profile does not
redefine these schemas; building the missing seams is the Walk/Run work below.

## Selecting a profile — model implies profile; `--profile` overrides

By default the **model implies the profile** — zero-config does the right thing:

```bash
newt --model nemotron3:33b                       # implied → nemotron profile
newt --model nemotron3:33b --profile default     # override → today's baseline harness
newt --model qwen3-coder:30b  --profile nemotron # override → the cross-application lever
```

`--model` and `--profile` are **net-new top-level CLI flags** on the `Cli` struct
(`newt-cli/src/lib.rs:22-135`) — neither exists today. They thread to the TUI the
way `--debug` / `--num-ctx` already do, via env vars in the `Code` dispatch
branch (`newt-cli/src/lib.rs:252-299`, e.g. `NEWT_PROFILE` / `NEWT_MODEL`), read
back in `run_code` (`newt-tui/src/lib.rs:83-122`) and applied in `run_chat`
(`newt-tui/src/lib.rs:2359-2433`) **before** `resolve_backend_choice(&cfg)` at
line 2423.

**Resolution order:**

1. **Explicit `--profile <name>`** wins. The profile is looked up in a **new**
   `Config.profiles: BTreeMap<String, ProfileConfig>` table (alongside `modes`,
   `newt-core/src/config.rs:26-122` — does not exist today) and its knobs override
   `cfg`.
2. **No `--profile`, model known → infer family by name prefix.** This is the
   default path: reuse the proven longest-prefix match already in `builtin_rate`
   (`newt-core/src/pricing.rs:92-125`): `nemotron3:33b` / `nemotron-3-nano:4b` →
   family `nemotron` → the `nemotron` profile. Today `find_model_tuning`
   (`newt-core/src/config.rs:653`) is **exact-match only**; the family fallback
   chain (`exact model → family prefix → default`) is a **gap**.
3. **Default.** With no inferable family, the `default` profile applies — today's
   behavior, unchanged. Inference is a *fallback*, never an override of an
   explicit flag; auto-selection stays conservative.

`--model <id>` independently overrides the backend choice at
`resolve_backend_config` / `resolve_backend_choice` (`newt-tui/src/lib.rs:3385-3409,
3425-3445`). A persona's `RoleProfile.model` (`newt-core/src/role_profile.rs:78`,
`pub model: Option<String>`) is already parsed but **never consulted** by backend
selection — a wasted seam profile resolution should wire as a composition tier
(persona model → family inference) rather than leave display-only. A mid-session
`/profile <name>` command (mirroring `/mode`) is a natural later addition, **not**
required for the first cut.

### The override is an instrument — the model × profile matrix

Cross-applying a profile across families is a **first-class research lever**, not
just a fallback for unknown models. The override answers questions a model-only
sweep can't:

- **`qwen-coder × nemotron`** — is the heavy help *inert or harmful* on a strong
  model? (Does forcing manifest + strict verify + decomposition cost a model that
  didn't need it?)
- **`nemotron × default`** — does nemotron *actually need* the help, or was the
  baseline already enough on some rolls? (The forensic says it needs it; the
  matrix quantifies by how much.)

This turns the sweep (#350) into a **model × profile matrix**: add a `--profile`
axis to the existing `--models` / `--repeats` sweep, and each cell measures
whether a knob earns its keep, *for whom*. The matrix is how a profile's value is
**proven**, not asserted — the same discipline as the per-cell pass-rate.

**Scope guardrail.** Profiles apply at the **harness layer**
(`newt-core::agentic`, verify gate, compression), never at the chat presentation
layer. Per [`plain_scroller_tui.md`](../decisions/plain_scroller_tui.md): no
per-family UI variation, no profile-name leakage into the chat surface, and the
headless tier (wyvern-agent) must strip profiles cleanly. A profile may tune
*which* advisories fire; it must not change disclosure *presentation* policy.

## Where profiles live

Profiles compose with the **Phase 20 auto-tuner**, which already persists
per-model tuning. The division of labor:

- **The sweep is the fitness function; the tuner discovers the values.** Profile
  knob values are not authored — they are *measured*. The Phase 20 machinery
  records per-model capabilities (`CapabilityEntry`, `newt-tui/src/probe.rs:72-159`)
  into `~/.newt/model-capabilities.json` (`CapabilityCache` load/save,
  `probe.rs:337-409`) through the observation hook (`RoundObservation` →
  `on_round_usage` → `apply_observation`, `newt-core/src/agentic/mod.rs:87-111,524`;
  `newt-tui/src/probe.rs:162-294`). That cache is the **per-session inner loop**.
- **A profile is the outer, family-wide layer.** Where `CapabilityEntry` is keyed
  by exact model, a profile is keyed by *family* and bundles the harness knobs
  (not just window numbers). The natural home is alongside existing per-model
  config: a `[profiles.<name>]` table (mirroring `[modes.<name>]`) and/or a
  section in the shareable `community-tunings.toml` (`TuningProfile` /
  `CommunityTunings`, `newt-core/src/tuning.rs:53-98,141-165`). The
  community-tuning format — shareable TOML, additive optional fields,
  confidence-ranked merge — is the right reference for shipping a `nemotron` pack
  a user can import (`newt tunings import`).
- **Composition.** A profile sets the *baseline* `AgenticConfig` at session load;
  Phase 20's `RoundObservation` feedback then **refines** it per session
  (calibration ratio, `max_ok_input` ratchet). Family profile = bootstrap;
  Phase 20 = online refinement. Writeback must tag origin so a swept family value
  never clobbers a hand-authored `[[model_tuning]]` entry (today only
  `CapabilityEntry.tune_confidence` records source — a known gap).

## The `nemotron` profile — worked example

The forensic implies concrete, evidence-backed knob values for `nemotron`,
contrasted with the lighter `qwen-coder` and `default` profiles. **These are the
*implied* settings the sweep is expected to confirm — not yet measured
constants.**

| Knob | `nemotron` | `qwen-coder` | `default` | Why (from the forensic) |
|---|---|---|---|---|
| FFI manifest (#74) | **on** | off | off | R1: the answer was evicted by compression; an 8-line `{crate → import_path}` manifest is the form that survives prune+summary. Highest leverage. |
| verify strictness | **strict** | normal | normal | The fabrication is only catchable by an import-resolving oracle (`Verdict::Fabricated`); blind `py_compile` passes it. |
| revert-retry `N` | **N ≥ 3** | 1 (off) | 1 (off) | R2: failure is sampling-stochastic, so a *fresh roll* has the pass-rate's chance; revert (not fix-in-place) avoids re-anchoring the prior. N attempts → ≈ `p^N`. `p` measured by the repeats sweep (#350). |
| re-ground after compress | **forced** (breadcrumb carries the fact, R4) | default | default | Both runs compressed; the PASS re-grounded, the FAIL didn't. Raise the pressure to recover the surface after a compress. |
| decomposition | **on** (cap working set per subtask) | off | off | nemotron overflows + confabulates at 8 crates ≈ 20k tokens; keep each subtask under the effective window. |
| window budget | conservative | model-proven | model-proven | qwen found the surface at the same context; nemotron benefits from a tighter, calibrated budget. |
| prompt / soul shape | nemotron framing (deferred) | default | default | R5 names per-family prompt tuning; scoped, not yet specified. |

`qwen-coder` is deliberately **light** — it resolved the umbrella at identical
context, so the heavy machinery is off and it inherits `default` with at most a
model-proven window. `default` is today's harness, unchanged — the safe baseline
auto-selection falls back to. The `qwen-coder × nemotron` cell of the matrix is
exactly what tells us whether that "light is correct" call holds.

## Staging — crawl / walk / run / fly

Per the binding work style (smallest tested landable increment first; build-free
and CI-testable before the load-bearing factor; do not fly too early):

- **Crawl** — a static `--profile <name>` flag selecting a **hardcoded preset
  bundle** in-binary (`nemotron` / `qwen-coder` / `default`), plus `--model`. Pure
  flag wiring through the existing env-var thread (`newt-cli/src/lib.rs` →
  `run_code` → `run_chat`), overriding `cfg` before `resolve_backend_choice`. No
  config schema, no persistence — unit-testable with `assert_cmd` + `predicates`,
  additive, and **behind a flag** (absent flag = today's behavior byte-for-byte).
- **Walk** — config-file profiles: add `Config.profiles: BTreeMap<String,
  ProfileConfig>` (`config.rs:26-122`) + a `ProfileConfig` struct, loaded through
  the existing layering, mirroring `[modes.<name>]`. Add the family-prefix
  fallback chain to `find_model_tuning`. Wire the persona `RoleProfile.model`
  seam. Values still hand-authored; the matrix validates them.
- **Run** — auto-discovered / tuned profiles: the Phase 20 tuner writes *measured*
  knob values into the profile (origin-tagged so hand-authored entries are never
  clobbered); profile = bootstrap, `RoundObservation` = refinement.
- **Fly** — nightly-swept per-family packs: the sweep wrapper + nightly CI runs
  the model × profile × corpus-size matrix across seeds, persists the winners as
  the family-profile discovery feed, and a harness change that helps one family
  while hurting another becomes visible in the scorecard diff.

**Constraints across all stages:** no advanced-TUI scope creep — profiles are
harness-layer only ([`plain_scroller_tui.md`](../decisions/plain_scroller_tui.md));
every stage is **additive and behind a flag**; and the **coverage floor ratchets
up, never down** — new profile code ships with tests that hold the floor.

## Roadmap slot + cross-links

Model-family profiles are the **outer, offline loop** to Phase 20's inner, online
loop, and slot as **Step 20.3** (`docs/ROADMAP.md`, Phase 20 begins line 1025;
existing 20.1/20.2 are the per-session feedback + `/probe` discovery). A profile
selects the family-wide baseline `AgenticConfig` at session load; 20.1's
`RoundObservation` feedback then refines it per session. Profiles **select** which
Phase 9.8 `AgenticConfig` to apply (`docs/ROADMAP.md` Step 9.8, line 583) — they
do not redefine its schema. (Sequencing the actual roadmap entry is the human
steward's call; Step 20.3 is the proposed slot.)

Related work this note coordinates:

- **#73** — verify-gated revert-and-retry (the verify-strictness + retry-`N` knob).
- **#74** — FFI-introspection manifest (the `nemotron` profile's highest-leverage
  knob; also upgrades the oracle from module- to symbol-level).
- **Phase 20** — model self-tuning ([`docs/design/model-self-tuning.md`](model-self-tuning.md);
  the tuner that discovers profile values).
- **#350** — repeats sweep, which measures the per-roll fabrication rate `p` (sets
  the revert-retry budget `N`) and grows the `--profile` axis into the matrix.
- **#319 / #321 / #332** — the incident, the re-read breadcrumb, and the verify
  oracle the profile knobs build on.
