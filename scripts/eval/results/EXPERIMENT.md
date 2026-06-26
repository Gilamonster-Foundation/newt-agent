# Autonomous #548 evaluator — experiment log

Measuring whether the autonomous `newt plan --one-shot` loop can actually
implement issue #548 (roll up the verbose `/dgx` help into one top-level line +
keep `/dgx help` as the progressive-disclosure detail page), and isolating the
effect of features landed on `main`.

## Method

- **Instrument (fixed):** `scripts/eval/grade-548.sh` — a *behavioral* grader. It
  drives a built `newt` (lean/pipe mode) and inspects the real `/help` output:
  - `top_dgx_subs` — `/dgx <sub>` lines at the top-level `/help` (rolled up ⇒ **≤1**)
  - `dgx_help_subs` — same under `/dgx help` (disclosure ⇒ **≥5**)
  - **PASS** ⇔ rolled up **and** disclosure kept.
  Why behavioral, not `just check`: run A produced a module that *compiled and
  passed* `just check` but was an orphan (never wired into `help_lines`) — the
  feature did not exist. `just check` is necessary, not sufficient.

- **Variable:** the codebase under test. Each run rebases the (fixed) grader onto
  a different commit, runs the identical `--one-shot` eval against a throwaway
  checkout, grades the result.

- **Runs:**
  - **A** — `68c9b2c` (baseline).
  - **B** — `41cb1de` = A + the **#661 compaction/summarizer series** (#666
    progressive-disclosure compaction, #667 summarizer→embedded-engine, #668
    knowledge-base compaction test).
  - **C** — pending (A + B + the next feature).

- **Constants:** prompt (the #548 URL + "come up with a plan to implement it"),
  authoring model `nemotron-3-nano:30b`, crew `[crews.home]` (planner dgx1,
  navigator/triage gnuc), `--max-leaves 12`, warm `CARGO_TARGET_DIR`.

> **Stochasticity caveat (read this).** The planner + crew are LLM-driven and
> non-deterministic. With **one trial per condition**, the *process* differences
> between runs (leaf counts, which files got touched) are within run-to-run
> noise and CANNOT be attributed to the features. The robust signal is the
> **grader outcome**; the deterministic regression check (below) is noise-free.
> Statistical attribution of a feature's effect needs multiple trials per cell.

## Results

### Deterministic regression check (noise-free)
Grading each codebase's *own* binary (no eval, no LLM) — does the codebase itself
change the #548 surface?

| Codebase | `top_dgx_subs` | `pass` |
|---|---|---|
| A `68c9b2c` | 8 | false |
| B `41cb1de` (+#661) | 8 | false |
| C `d25662d` (+#669) | 8 | false |

➡ **No regression in any.** Neither the #661 series nor #669 touched the `/dgx`
help output; all three correctly FAIL (the rollup is implemented in none).

### Autonomous eval (the stochastic part)

| | **A** (`68c9b2c`) | **B** (`+#661`) | **C** (`+#669`) |
|---|---|---|---|
| Loop completed | ✅ "✓ complete" | ✅ "✓ complete" | ✅ "✓ complete" |
| Leaves (planned = done) | 9 | 11 | 9 |
| Wall-clock | not captured | **9103 s (~2.5 h)** | **7266 s (~2.0 h)** |
| Plan named `help_lines`/`newt-tui`? | ❌ (orphan module) | ✅ (mis-located to `crew.rs`) | — |
| Net change produced | `+66` orphan `dgx_help.rs` | README `−211` + Cargo tweak | **nothing** (9/9 leaves no-op) |
| Touched real `help_lines`? | ❌ (0 lines) | ❌ (0 lines) | ❌ (0 lines) |
| **Grader `top_dgx_subs`** | **8** | **8** | **8** |
| **Grader `pass`** | **false** | **false** | **false** |

```
top_dgx_subs   (lower is better; 0 = rolled up / PASS, 8 = baseline / FAIL)
  A  ████████  8   FAIL   (orphan module)
  B  ████████  8   FAIL   (gutted README)
  C  ████████  8   FAIL   (no changes at all)
  ▏  0  ← target (PASS)

implemented #548 (pass)?     A: ✗     B: ✗     C: ✗     (0 / 3)
```

## Learnings (A↔B↔C)

1. **No feature moved the needle.** A, B, and C grade **identically** (`top_dgx_subs
   8`, `pass false`). Neither the #661 compaction/summarizer series nor #669's
   workspace-API knowledge base changed the autonomous #548 outcome. They manage
   context; #548 is too small to lean on them, and they don't touch the actual
   bottleneck.
2. **The bottleneck is crew-implementation quality — high and unmoved.** Three
   runs, three distinct *non*-implementations: A wrote an orphan module, B gutted
   the README, C produced literally nothing (9/9 leaves no-op). The grounding (gh +
   repo + grep, byte-identical across runs) gets the planner to the right
   neighborhood; the crew never wires the real `help_lines`.
3. **`just check` hides non-implementations — proven 3×.** An orphan module, a
   README gut, and a no-op all "pass." The behavioral grader is what separates
   "the loop completed" from "the feature exists." `✓ plan complete` ≠ `#548 done`.
4. **More context ≠ more action (suggestive).** C, with the *most* context (the
   workspace-API knowledge base), was the *most* conservative — every leaf no-op'd.
   One trial, so noise — but worth watching whether richer context makes the crew
   timid rather than capable.
5. **Cost:** ~2–2.5 h per run (9–11 leaves × a `just check` build each); per-leaf
   builds dominate. C was fastest (fewer leaves, all no-op).

## Bottom line
Across three codebase versions the autonomous loop is **mechanically robust**
(every run completes + consolidates) but **0 / 3 on actually implementing #548**.
The landed features (#661, #669) are orthogonal to that outcome. The next lever is
unambiguously **crew-implementation capability** (a stronger crew model / a tighter
navigate→edit→wire-in→verify loop), *not* more planning context. The behavioral
grader now makes that measurable: re-run it against a stronger crew and watch
`top_dgx_subs` move off 8.

## Reproduce
```
./scripts/eval/grade-548.sh <newt-binary>                 # behavioral grade
newt plan --goal "<#548 url> … implement it" --one-shot --dir <throwaway>  # eval
```
Raw per-run logs + diffs: `scripts/eval/results/`.
