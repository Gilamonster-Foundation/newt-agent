# Test & implementation plan — driving local-model tasks to done (dgx1 inference)

Companion to [`next-loop-levers.md`](../next-loop-levers.md) (the decision menu)
and [`next-loop-levers-yardstick.md`](next-loop-levers-yardstick.md) (the
verbatim fixture + incident config). This doc is the **how we measure it** —
the instrument, the substrate, and the per-lever cells — so each lever earns a
sweep verdict before it's called a grade-mover (`#803`: plausible ≠ measured).

**Shape:** reuse the proven n≥5 sweep discipline (`sweep.sh`/`ratchet.sh`) with
a **new loop-completion grader** that reads the yardstick's signals out of each
run's own `conversations.db`. Independent variable = one lever per cell;
`ornith:35b` on dgx1 is the fixed inference substrate. **Baseline-first**:
reproduce the failure at n≥5 before flipping anything (an instrument that
doesn't fail on the baseline is miscalibrated — same rule as
`scripts/eval/METHODOLOGY.md`).

## 1. The instrument

Two new scripts, siblings of `grade-548.sh`/`sweep.sh`:

- **`scripts/eval/grade-loop.sh`** — drives a built `newt` in pipe mode
  (`--plain`, no `--ephemeral`) through the verbatim yardstick prompts against an
  **isolated `$HOME`** (newt roots its data dir at `$HOME/.newt` —
  `newt-core/src/config.rs` `home_dir()`; `store.rs` `DB_FILE`), so the run
  writes its own `conversations.db` and never touches the operator's `~/.newt`.
  It then extracts the yardstick's "Scoring a rerun" signals and emits one JSON
  verdict line. A trial that persisted nothing (backend unreachable /
  permission-blocked) is an **ERROR**, never a silent FAIL.

  | Signal | Source | Incident (fail) | Pass condition |
  |---|---|---|---|
  | plan ledger | `conversations.plan ≠ '{}'` | `{}` | present |
  | cap hit / salvage | `assistant LIKE 'reached the tool-call limit of…'` / `'Progress captured'` (`mod.rs:3417-3435`) | banner, no salvage | not capped, or salvaged |
  | dangling narration | last turn tail matches `let me…$` | yes | no |
  | phantom reaches | `turns.phantom_reaches` + `usage.jsonl` | up to 4 | ↓ (RC5 metric) |
  | max events | `MAX(json_array_length(events))` | 25 | — |

  **PASS ⇔ plan ledger exists AND ending is not a dangling narration AND the
  turn was not capped with empty salvage.** All raw signals are in the JSON so
  PASS can be redefined downstream.

- **`scripts/eval/loop-sweep.sh`** — runs `grade-loop.sh` n≥5 per cell with
  crash-resume, append-only results, a baseline-relative `/ab-gate` verdict
  (`MOVED +Npp` only at n≥5), `--status`/`--reap`/`--self-test`, and
  `systemd-run --user` detach. Config levers (T0.1, T0.2) toggle within one
  binary; code levers (L1…) rebuild `newt` and sweep with `--newt <that-binary>
  --levers baseline` (cross-binary A/B).

- **`scripts/eval/loop-template.example`** — the local, uncommitted endpoint
  template. dgx1's endpoint lives only here; nothing under `results/` records a
  host. The `[tui.permissions]` block must grant non-interactive access (no
  human in a pipe drive).

Both scripts are `shellcheck`-clean and carry offline `--self-test`s (fabricated
DBs / pure aggregation math) so CI validates them without dgx1.

## 2. Substrate — dgx1

`ornith:35b` served by Ollama on dgx1; the model **card** ships in-repo
(`newt dgx card show`, #854), so only the weights are host-side. The sweep
substitutes `{{MODEL}}=ornith:35b`; the host is read from the local template.
GPU strongly recommended for a 35B. **Contention is a real confound** — the
incident host was serving other agents, which is what starved the mid-loop
summarizer (§2.4); an idle dgx1 softens that leg. Run each summarizer-touching
cell (baseline, T0.2, L5) **both** idle and under synthetic load, and record
which.

## 3. Baseline gate (do this first)

Build `v0.7.1` **feature-off**, exactly as the incident (the missing `embedded`
feature is part of the baseline):

```bash
git clone https://github.com/Gilamonster-Foundation/newt-agent && cd newt-agent
git checkout v0.7.1 && just install ~/bin        # plain install = feature-off
cp scripts/eval/loop-template.example ~/.newt/eval-sweeps/ && $EDITOR ~/.newt/eval-sweeps/…  # add dgx1 host
scripts/eval/loop-sweep.sh \
  --out scripts/eval/results/loop-sweeps/baseline \
  --newt ~/bin/newt --model ornith:35b \
  --levers baseline --trials 5 --scratch /var/tmp/loop-sweep \
  --workdir /path/to/cloned/newt-agent
```

**Gate:** the failure signature (cap banner + empty salvage, and a dangling
`continue`) must reproduce reliably. If it doesn't, stop — the substrate or the
permission config is off (`grade-loop.sh` ERRORs loudly if the drive stalls).

## 4. Per-lever cells

| Lever | Impl scope | Build/config | Primary metric | Expected move | Prereq |
|---|---|---|---|---|---|
| **T0.1** unpin cap | config | `max_tool_rounds 25→40` | completion, rounds-to-done | small alone ("thrash longer", `ROADMAP.md:1326`); real only *with* L1 | none |
| **T0.2** summarizer off session | config | drop `loop-summarizer.toml` in | compaction backend, static-marker rate | ↓ empty summaries; **cannot** fix cap-exit (see §6) | none |
| **T0.3** probe | config | `/probe ornith:35b` | conformance datum | measurement, not a grade-mover | none |
| **L1** plan-by-round-N gate ⭐ | `mod.rs:1302-1322`/`1736-1858` | binary | **plan-ledger → ~100%**, then grace/salvage/completion ↑ | largest single move | build |
| **L6** breadcrumb→handle | `compress.rs:1087-1133` + advertise `memory_fetch` (`lib.rs:6379-6407`) | binary | fetch-vs-reread, re-read rounds | ↓ re-exploration | build |
| **L5** embedded summarizer default | summarizer resolve + `newt models pull` | `--features embedded` + palette GGUF | compaction on-embedded, 0 dgx1 contention | removes contention leg | **rebuild + fetch GGUF** |
| **L3** narration_nudge_cap | `mod.rs:3020` | binary, cap 1→2→3 | dangling-narration rate | ↓ stalls | build |
| **L2** no-plan grace arm | `mod.rs:2723-2762`, `1231` | binary | grace on plan-less-but-progressing | completion for read-heavy turns; pays off *after* L1 | build |
| **L4** honest cap-exit | `mod.rs:3424-3447`, `2933-2941` | binary | salvage-non-empty → 100% | turns stop being total losses | build |
| **F1** plan_mode+effort | new driver (#385 `<think>` split) | binary | plan-quality + completion, plan survives compaction | the "better plans" answer | #385 |
| **F2** disclosure compaction | Step 20.4 (`compaction_mode`) | binary | re-read rounds → ~0 | never-lose | **L6 + memory_fetch advertise** |
| **F3** auto-continue | new `auto_continue_turns` | binary, off-by-default | bounded manual-continue automated | sharpest; multiplies thrash | **L1+L2** |

## 5. Sequencing (build order, prereqs baked in)

1. **Config-only cells on the feature-off binary** — baseline, T0.1, T0.2,
   T0.3. No rebuild; runnable on dgx1 immediately.
2. **One binary per code lever, one sweep each vs baseline:** L1 → L6 → L3 →
   L2 → L4. L1 first because every downstream metric (grace, salvage, re-seat)
   is plan-keyed — L1 is the gate that makes them reachable (the "only fails
   when a plan is needed" observation).
3. **L5** after `cargo clean` → `just install ~/bin newt-agent/embedded` →
   hand-fetch `qwen2.5-1.5b-instruct-q4_k_m.gguf` + `tokenizer.json`.
   ⚠️ **Do not** set `kind="embedded"` on any earlier cell's feature-off
   binary — feature-off + `embedded` = a permanently-failing summarizer with no
   HTTP fallback (`lib.rs:4290-4297`, `compress.rs:1390-1398`).
4. **F-tier** last: F1 (needs #385), F2 (needs L6 + the `memory_fetch` advertise
   fix), F3 (only after L1+L2 exist to gate it), F4 wraps winners into
   per-family profiles.

## 6. Confounds & controls (do not skip)

- **The cap-exit path is separate.** `final_summary_ollama` always uses the
  *session* client (`mod.rs:2377-2378`), so **no `summarizer.toml` and no L5 can
  fix the "final summarization failed" cap-exit turns** — that is dgx1
  contention on the cap-exit summary. `grade-loop.sh` records it distinctly; it
  likely needs its own lever (route cap-exit summary through the summarizer /
  embedded backend). Worth adding to the menu.
- **RC5 has no lever yet.** Tool-name-as-command hallucinations burn ~8/25
  rounds and nothing prevents/refunds them (`mod.rs:1240` — "no refund path").
  The grader tracks `phantom_reaches`/`hallucinations` precisely so we can size
  it; a candidate lever (don't spend a full round on a detected
  `is_hallucination`) belongs on the menu.
- **Session A is private.** `scrybe#37` can't be fetched on a clean box —
  substitute one fixed public issue URL, held constant across runs. Session B
  (#969) is fully public and is the primary fixture.
- **Permissions / disk.** The pipe drive needs non-interactive auto-grant (see
  the template). L5's build needs headroom — the box runs ~12 GiB free;
  `cargo clean` in `target/` reclaims ~18 GB.

## 7. Status

- ✅ `grade-loop.sh`, `loop-sweep.sh`, `loop-template.example` — landed, shellcheck-clean, self-tested.
- ⏳ Baseline sweep on dgx1 (§3) — run on the box with dgx1 reachability.
- ⏳ Per-lever cells (§4) — after baseline calibrates.
