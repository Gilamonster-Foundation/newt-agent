# Decision: the `/plan` editor is an ephemeral alt-screen surface (carve-out #2)

**Status:** Accepted (decided by Shawn Hartsock, 2026-06-13)
**Date:** 2026-06-13
**Supersedes:** nothing. This **adds a second standing carve-out** to
`docs/decisions/plain_scroller_tui.md`; that decision stands intact.
**Related:** `docs/decisions/plain_scroller_tui.md` (the plain-scroller rule
and its revisit trigger), #334 (the collaborative `/plan` feature this surface
serves), #332 (the umbrella: carve large tasks into small grounded verified
ones), #177 (`/plan` mode), #302 (splash leaks raw mode/alt screen on error —
the teardown bug this surface must not repeat), `docs/decisions/mesh_integration.md`
(newt as a mesh worker; wyvern flies headless).

---

## TL;DR

The `/plan` collaborative planner stores its plan as a **plain-text TOML
front-matter document** (`.scratch/sessions/<id>/plan.md`) whose `[[subtask]]`
table is the swarm `Plan`/`Subtask` struct (see #332, "Option C"). That format
is the most powerful and the only one that lifts single-agent → swarm without a
rewrite — but it is **hostile to hand-edit**. To let a human edit it without
brain-pain, newt grows **one more ephemeral alternate-screen surface**: a small
`/plan` editor, opened **only on explicit human request**, that lets a person
toggle/reorder/edit steps as a form instead of hand-writing TOML.

This is **carve-out #2**, identical in shape to the existing carve-out #1 (the
startup splash): it lives in the alt screen, is feature-gated out of headless
builds, restores the terminal with an RAII guard even on error, and — on close
— **prints the canonical plan into real scrollback exactly as a headless run
would**. The chat path stays a plain scroller. The plain-text plan document
remains the single source of truth, and is the **handoff artifact** a
UX-bearing agent (newt / drake) passes to a **headless wyvern** that ingests it
from the CLI and never opens an editor.

## Why this clears the plain-scroller decision's bar

`plain_scroller_tui.md` does not forbid alt-screen surfaces absolutely — it
forbids them **in the chat path**, lists the splash as a standing carve-out, and
sets an explicit **revisit trigger**:

> "if newt genuinely cannot express a needed interaction as scrolled lines,
> write a new decision doc that supersedes this one — do not land the surface
> change first."

Hand-editing a dependency-bearing subtask DAG is that interaction: a scroller
can *print* a plan, but it cannot offer toggle/reorder/field-edit over a
structured document without becoming a redraw loop. So we follow the prescribed
process — **this doc lands before any editor code** — and we keep the surface
inside the same envelope the splash already proved safe.

## The collaborative `/plan` flow (human-driven on purpose)

newt runs **small** local models. The plan is therefore **more human-driven**
than a large-model "plan mode": the human stays in the loop to catch the
decomposition errors a 14k-effective-window model makes (see #332). The flow,
in tiers of escalating interactivity, all **default to the plain scroller**:

1. **Propose** — `/plan <task>` reads the codebase under a read-only clamp and
   prints a structured plan as **plain scroller lines** (headless parity: a
   wyvern run prints the same lines to its log).
2. **Discuss / refine** — the human and agent **talk through the plan in the
   normal chat scroller**: "merge steps 3 and 4", "add a verify step", "this
   step needs `__init__.py` in its context". The agent revises and reprints.
   This conversational loop is plain-scroller only — **no alt screen** — and is
   the primary collaboration surface. The editor is the exception, not the rule.
3. **Edit (optional, the carve-out)** — when fine-grained structural editing is
   easier as a form than as prose, the human types `/plan edit` (or answers the
   `[y / N / discuss / edit]` approval prompt with `edit`) to open the
   **ephemeral alt-screen editor**. Toggle include/skip, reorder, edit
   instruction / context files / verify command. `E` drops the whole `plan.md`
   into `$EDITOR` (vi) for a long free-text edit. On save it serializes back to
   the canonical TOML.
4. **Approve** — the plan is written to `.scratch/sessions/<id>/plan.md`.
5. **Execute** — one subtask per turn, each in a fresh narrow context (#332 S3).

Steps 1, 2, 4, 5 never touch the alt screen. Step 3 is the only new surface, and
it is **opt-in and human-initiated**.

## Decision

1. **Add carve-out #2: the `/plan` editor**, an ephemeral alternate-screen
   surface, to the standing-carve-outs list in `plain_scroller_tui.md`.
2. **It is human-only and explicitly entered.** It is reachable solely via an
   interactive `/plan edit` request. No code path, and no headless/wyvern build,
   opens it implicitly. The propose/discuss/approve/execute path is plain
   scroller.
3. **The plain-text plan document is the source of truth.** The editor mutates
   an in-memory `Plan` and serializes to `.scratch/sessions/<id>/plan.md`. The file
   is fully usable without the editor — editable by hand in vi, over a dumb
   terminal, or generated headlessly. The editor adds zero capability that the
   file does not already express.
4. **Strippability is mandatory (plain-scroller decision #4).** The editor lives
   behind a cargo feature (the same gate family as the splash) and is **absent
   from the wyvern headless build**. Removing the feature must leave a working
   headless `/plan` that produces and consumes the same `plan.md`.
5. **On close it leaves a clean typescript.** Teardown restores scrollback and
   prints the approved plan using the **same renderer the headless `/plan`
   uses** — so the transcript is identical whether or not the editor was opened.
   *What the human tested == what the swarm runs.*
6. **RAII teardown is mandatory (do not repeat #302).** The terminal must be
   restored (`disable_raw_mode` + `LeaveAlternateScreen` + show cursor) by a
   `Drop` guard that runs on the error and panic paths, not only the happy path.
   #302's splash leak is fixed with the same guard and the guard is **shared
   infrastructure**, so newt has exactly one audited terminal-restore path.

### What is still forbidden (unchanged)

No panes, splits, persistent status bars, live-updating dashboards, mouse
handling, or raw-mode event loops **in the chat path**. No alt-screen surface
other than the splash (carve-out #1) and this editor (carve-out #2). Richer
plan UIs — tree views, drag-to-reprioritize, inline branch status — remain
**gilamonster-agent / monitor-agent** territory, per the plain-scroller
decision. If the editor starts wanting those, that is the signal it belongs
there, not here.

## The amphibious / cross-tier boundary (wyvern ingestion)

The plan document is the **interchange format across the tier boundary**, and
this surface decision is what keeps the boundary honest:

- **UX-bearing agents author and edit** the plan. newt (human at the keyboard)
  and, later, drake (swarm control plane) run propose → discuss → edit →
  approve. The editor lives here.
- **Headless wyvern ingests, never edits.** wyvern flies with **no UX** and
  speaks only agent-mesh; it cannot open an editor and has no human to ask. It
  receives a plan **from a UX-bearing agent** and runs it. Therefore the plan
  must be ingestable headlessly from the CLI:
  - a **whole plan** (`--plan plan.md` / stdin), or
  - a **plan fragment** — a subset of `[[subtask]]` entries (the slice assigned
    to one flight), composable without the surrounding `goal`. The TOML schema
    must be **fragment-valid**: a bare subtask list parses on its own.
- Because the editor only ever *produces the same file* a human could hand-write
  or an agent could emit, the headless tier is never coupled to the surface. The
  editor is a convenience over the wire format, not a second channel.

This is the plain-scroller decision's "amphibious" property applied to plans:
the same plan document drives a human-edited newt session and a headless wyvern
flight, and the alt-screen editor is the strippable human affordance — exactly
parallel to how `--disable-ocap` is the human-only authority affordance.

## Consequences

- **PR-review guidance:** an `EnterAlternateScreen` / ratatui / raw-mode loop is
  still rejected anywhere **except** the splash and the `/plan` editor, both
  feature-gated, both RAII-torn-down. A live-updating plan dashboard, or an
  editor that opens without explicit human request, is rejected or redirected to
  gilamonster-agent.
- **#302 becomes a dependency, not a footnote.** The shared terminal-restore
  guard is built/closed as part of this work; the editor must not ship before
  the guard exists. A regression test must assert the terminal is restored when
  the editor body returns `Err` and when it panics.
- **Headless test parity:** the renderer that prints the plan to scrollback on
  editor-close is the same function headless `/plan` calls; a test asserts the
  two outputs are byte-identical for a fixed plan.
- **`newt-tui` crate description** still overstates ratatui's role
  ("code mode + pilot mode"); reword when next touched (carried over from the
  plain-scroller decision) — it now hosts splash + plan-editor, still not a
  general TUI.

## Revisit trigger

If the plan editor cannot express needed editing as a single ephemeral
alt-screen form — e.g. it grows toward persistent panes, live status, or a
multi-document workspace — that is the signal the richer planner belongs in
**gilamonster-agent**, not that newt should grow a third surface. Write a new
decision before landing it.
