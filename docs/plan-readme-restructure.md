# Plan — break up README.md, scoreboard front and center

**Status:** proposed · **Branch:** `readme-restructure/scoreboard-first`

## The thesis

The README's job is to answer, in the first screen: *what is this, and is it any
good?* Today the answer to "is it any good?" is on line 101 — below the fold,
under three sections of configuration detail (scrollback spill rows, `/mode`
semantics) that only matter to someone already running the binary.

The Terminal-Bench scoreboard is the bragging right. It goes first. Everything
that is *reference material for an existing user* moves out to `docs/`, and the
README becomes a front door plus an index.

The telescope check: the README is not about newt's feature surface. It's about
**whether a local, confined agent can do real work** — which is exactly one
number, measured honestly, by an instrument that doesn't belong to us.

## Current shape (199 lines, `origin/main` @ 7d870b4)

| Lines | Section | Disposition |
|---|---|---|
| 1–12 | Title, logo, tagline, one-para what-it-is | **Keep**, tighten |
| 14–36 | Why — the bridle, not just the harness | **Trim** to ~6 lines → `docs/why-a-bridle.md` |
| 38–58 | Quick start | **Keep** the 5 commands; auth/endpoint detail → guide |
| 60–75 | Tool output scrollback | **Move** → `newt-tui/README.md` (already linked from here) |
| 77–99 | Operating modes and permission postures | **Move** → `docs/guide/modes-and-postures.md` |
| 101–139 | Terminal-Bench scoreboard | **Promote to the top**; drop queued rows |
| 141–166 | Design laws | **Keep** — short, and every bullet already links its ADR |
| 168–184 | Field notes | **Trim** to 2 entries + link → `docs/notes/README.md` index |
| 186–195 | Where things live | **Keep and grow** — it becomes the index for what moved |
| 197–199 | License | Keep |

Target: **~85 lines**, scoreboard visible without scrolling.

## Proposed README order

1. Title · logo · tagline
2. One paragraph: single Rust binary, local-first, opinionated
3. **`## Terminal-Bench`** — the trimmed table + 3 links (full results,
   methodology, upstream benchmark)
4. `## Quick start` — 5 commands, one note, link to the full setup guide
5. `## Why a bridle` — ~6 lines + link
6. `## Design laws` — unchanged
7. `## Field notes` — 2 entries + link to the index
8. `## Where things live` — the index
9. `## License`

## The scoreboard section, concretely

```markdown
## Terminal-Bench

Measured on [Terminal-Bench](https://github.com/harbor-framework/terminal-bench)
via `newt solve` (headless) + the Harbor adapter, by
[gilamonster-bench](https://github.com/Gilamonster-Foundation/gilamonster-bench)
— a separate instrument with no dependency on newt. The release gate is a
**per-model monotonic ratchet**: a model's score never goes down across releases.

<!-- BENCH-SCOREBOARD:START -->
| Model | OCAP off | OCAP on |
… measured rows only …
<!-- BENCH-SCOREBOARD:END -->

**Full results, every model, and the methodology →**
gilamonster-bench/results/newt-agent ·
[how the numbers were kept honest](findings/2026-07-29-dgx-spark-terminal-bench-survey.md)
```

Three rules for this block:

- **Queued rows are dropped** — a row that has never run is a to-do list, not a
  result. It belongs in the full table.
- **Keep both lanes.** OCAP-off vs OCAP-on *is* the interesting claim
  (confinement is nearly free). Collapsing to one number throws away the point.
- **Keep the `_pending_` cells.** A blank would read as zero. A labelled absence
  is honest; a suppressed one is not — the same lesson as
  [summarization-induced hallucination](notes/2026-06-13-summarization-induced-hallucination.md).

## Where the full results live

The user's call: **full benchmark results land in `gilamonster-bench`.** That is
right, and it needs one guard rail.

`gilamonster-bench`'s own README states the independence doctrine: *"If the ruler
and the thing it measures ship in the same release, one commit can move both at
once."* Publishing **results** there does not violate that — results are the
instrument's output, and that is exactly where an instrument's readings belong.
What would violate it is a *code* dependency in either direction. So:

- Results are **data** in the bench repo: `results/newt-agent/tb-30/results.jsonl`
  plus a rendered `results/newt-agent/SCOREBOARD.md` (full table, queued rows
  included, per-run provenance: digest, ctx, date, version).
- The **renderer moves too.** `scripts/eval/bench_scoreboard.py` (563 lines) is
  instrument code living in the measured repo — the exact entanglement the doctrine
  warns about. gilamonster-bench already emits scoreboard JSONL from
  `ingest --manifest`; it grows a `scoreboard render` subcommand and
  `bench_scoreboard.py` is deleted.
- newt-agent keeps **only** a thin step that pulls the published trimmed table and
  injects it between the existing markers. No scoring logic in the measured repo.

Alternative considered and rejected: a third `*-results` repo. It adds a hop
without adding independence — the bench already can't depend on newt.

## Staging — one issue, one PR

Each lands green on its own; none blocks the next.

| # | PR | Repo | Risk | Status |
|---|---|---|---|---|
| 1–3 | Renderer `--no-queued` + extraction + reorder | newt-agent | low | **done** — this branch |
| 4 | `results/newt-agent/` + `scoreboard render` in the bench; publish the full table | gilamonster-bench | low | next |
| 5 | Delete `bench_scoreboard.py`; newt-agent consumes the published table | newt-agent | **high** — cross-repo cutover | after 4 is proven |

**1–3 were collapsed into one PR** (authorized 2026-08-03). They all rewrite
`README.md`, and the table is auto-generated between markers — so shipping the
reorder without the renderer flag would let the next `just bench-publish` put the
queued rows straight back. Stacking three PRs on one file also walks into the
[stacked-merge hazard](https://github.com/Gilamonster-Foundation/newt-agent)
where merging a base with `--delete-branch` closes its children.

### What 1–3 actually needed

Less than planned. Three of the four proposed new docs already existed, and the
reuse discipline says widen the existing one rather than stand a second beside it:

- **Scrollback** — `newt-tui/README.md` already documented spills *more* fully
  than the README did. The README section was duplicate content: deleted, linked.
- **Modes/postures** — `docs/decisions/operating_modes_and_permission_postures.md`
  already covers both. Trimmed to two sentences + the link.
- **Why a bridle** — `docs/vision.md` (177 lines) is the long form. Trimmed and
  linked; no `docs/why-a-bridle.md` created.
- **Setup** — the only genuine gap. New: `docs/guide/setup.md`.

Result: 199 → 142 lines, scoreboard at line 14.

## Two things to fix while we're in here

- **Private hostname on a public README.** The old quick start ran `newt setup`
  against a real internal inference host by name. That name belongs in local,
  uncommitted config only — never in a script, a test, or a commit. Replaced with
  the `inference.example.net` placeholder.
- **`## Design laws` should name the scoreboard's ratchet.** "Patch, not prose"
  and the monotonic bench ratchet are the same law wearing two hats: verify by
  artifact, never by self-report. Worth one cross-link.

## Explicitly out of scope

- Rewriting the design-law or field-note prose. This is a *structural* break-up;
  content edits are a separate change with separate review.
- Any change to what is measured, how, or to the ratchet gate itself.
