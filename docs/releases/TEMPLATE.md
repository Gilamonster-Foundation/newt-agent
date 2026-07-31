# Release <VERSION> — <YYYY-MM-DD>

> Every release leaves a reproducible benchmark record here — the *witnessed* half
> of the champion-defeat ceremony. **Copy this file to `docs/releases/<version>.md`
> and fill it out on every release run.** Never hand-wave a number: a score belongs
> here only if it survives the honesty classifier **and** is pinned to the exact
> bits that produced it.

**Tag:** `v<VERSION>` · **Commit:** `<short-sha>` · **Newt binary/image digest:** `<sha256 | n/a>`

## Benchmark ceremony

**Suite:** tb-30 · **Context window:** `<N>` · **Tenacity:** `<default | …>` · **Lane:** confined (OCAP-on) · **Honesty floor:** ≥25/30 real attempts (else quarantined)

### Confined (OCAP-on) champions

Snapshot of the README scoreboard at release time (`just bench-publish` renders the live one). Same 3-column shape; metadata folds under the model name.

| Model | OCAP off | OCAP on |
|-------|----------|---------|
| `<model>`<br><sub><family>·tb-30·ctx<N>·v<VERSION>·<date></sub> | `<off | _pending_>` | `<on>` |

_Hosted controls (harness-ceiling comparison, no local weights):_ `<model>` `<score>`.

### Provenance — weights under test

A model NAME is not an identity (cf. the Gemma 4 2026-07-15 silent HF re-upload).
Every local score is pinned to the served GGUF's sha256.

| Model | GGUF sha256 |
|-------|-------------|
| `<model>` | `sha256:<…>` |

### Quarantined (measured but NOT scored)

Runs where the model failed to make a real tool-call attempt on ≥25/30 tasks — not
a capability score, so deferred rather than banked.

| Model | Real attempts | Reason |
|-------|---------------|--------|
| `<model>` | `<n>`/30 | `<under-engagement — see #…>` |

## Notes

- **Methodology:** `<honesty classifier; digest pinning; anything changed vs the prior release>`
- **Framing:** `<champion-defeat vs re-baseline — be explicit when the methodology changed, so a number is never conflated across measurement regimes>`
- **Caveats:** tb-30 is a 30-task sample; per-model numbers carry ~±10 pp run-to-run variance.

## Release checklist

- [ ] Confined runs ingested + digest-pinned (`scripts/eval/bench-results.jsonl`)
- [ ] `just bench-publish` (README scoreboard rendered)
- [ ] Version bumped: workspace package **+** internal `=x.y.z` pins **+** `Cargo.lock`
- [ ] `CHANGELOG` cut `[Unreleased] → [<VERSION>]`
- [ ] Release PR green + merged to `main`
- [ ] `release/<VERSION>` staging build green (binaries + packages, **no publish**)
- [ ] This record filled + committed
- [ ] `v<VERSION>` tag pushed → crates.io + PyPI publish
