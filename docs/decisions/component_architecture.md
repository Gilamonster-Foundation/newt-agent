# Decision: the component architecture — Markdown as HTML 1.0

**Status:** proposed, for discussion. Nothing here is implemented by this PR.
**Date:** 2026-09-01
**Answers:** #510 *"Crate and PyO3 decomposition and reusability — Newt Parts
for the world!"*, which asks verbatim *"can we export our vi, emac, and nano
TUI components to share ... a agent-prompt crate?"* and *"can we export our
context and attention management tools? an agent-context-manager crate?"*
**Related:** `agent_line_architecture.md` (authoritative on wyvern ≤ newt ≤
gilamonster), `esc_and_vi_contract.md`, `key_ladder_crate.md`,
`conversation_context_architecture.md`, `plain_scroller_tui.md`,
`newt_web_htmx.md`.

---

## 1. The thesis

From the operator, verbatim:

> We're turning Markdown into a kind of HTMX surface. The Markdown becomes
> essentially the equivalent of HTML 1.0 that is rendered by "browsers" into
> basic form interactions — a TUI, an HTMX, a Phone app. The equivalent of a
> CSS can re-theme the core system. Very much like the old "Zen of CSS" books.

That is not a metaphor for how the code feels. It is a description of four
layers, three of which already exist and one of which does not.

```mermaid
graph TD
    subgraph DOC["document — the HTML 1.0"]
        MK["newt-core::markup<br/>spans · dialect · table · extension<br/>2,781 lines"]
        IX["newt-interaction<br/>InteractionDefinition · SemanticRole<br/>ControlKind · Requirement<br/>2,960 lines"]
    end

    subgraph SHEET["stylesheet — the CSS"]
        TH["theme tokens<br/>DOES NOT EXIST YET"]
    end

    subgraph BROWSERS["browsers — one per surface"]
        PL["plain projection<br/>675 lines"]
        SP["span projection<br/>463 lines"]
        HL["headless projection<br/>451 lines"]
        WB["newt-web<br/>pulldown → ammonia → HTMX"]
        PH["phone app<br/>not built"]
    end

    MK --> PL
    MK --> SP
    MK --> HL
    MK --> WB
    IX --> PL
    IX --> SP
    IX --> WB
    TH -.->|"no binding today"| PL
    TH -.->|"no binding today"| SP
    TH -.->|"no binding today"| WB
    TH -.-> PH
    MK --> PH

    classDef missing stroke-dasharray: 5 4;
    class TH,PH missing
```

**Why this framing earns its place:** it corrects a carve we would otherwise
have made wrong. Markdown *renderers* were assessed and correctly left alone —
`termimad`, `tui-markdown`, `ratskin` and `comrak` all ship, and we have five
implementations across three different `pulldown-cmark` majors. But a renderer
**is a browser**, and browsers are per-surface by nature. What is shareable is
the **document model** and the **stylesheet**. "Extract the widgets" was the
wrong question; "publish the document, the forms and the sheet, and let each
surface keep its own browser" is the right one.

### The acceptance test — Zen Garden

> Take one `InteractionDefinition`, swap **only** the stylesheet, and get a
> different presentation **on the same surface**, with the document and the
> renderer untouched.

If that cannot be demonstrated, the layers are not separated, whatever the
module boundaries claim. Today it demonstrably cannot: there is no sheet to
swap, and colours are hardcoded per renderer across `newt-tui/`'s `brand.rs`,
`splash.rs`, `config_panel.rs`, `transcript_pager.rs` and `live_spill.rs`.

---

## 2. Where the seams are, and which ones hold

A seam counts as **real** only when something on both sides of it already
compiles against it. Measured at `be5647e5`:

| Seam | What crosses | State |
|---|---|---|
| `TurnDriver` | `submit` · `poll` · `cancel` · `submit_observation` | real |
| `InteractionDefinition` | one typed form → plain, rich TUI, and web | real |
| `Caveats` | attenuation-only authority, never amplification | real |
| `AdmittedServer` | MCP admission as a type witness | real |
| `RegionLease` | terminal rows as a capability | real |
| `precedence-ladder` | which claimant owns a key right now | real, published `0.1.0-rc.1` |
| card → family | typed metadata into the effort dials, never the model name | real |
| `CompactionSpan` | derived context provably tied to its source | real |
| **context manager** | **nothing, on cowork drives** | **broken** |
| **`newt-tui` exports** | **`run_code`, and nothing else** | **broken** |
| **`newt-cli` dispatch** | **public, and unconsumed** | **broken** |
| theme tokens | — | does not exist |

The three broken rows are not architectural failures; they are **missing
exports**. The layering is sound and the foundation crates are published. What
is absent is the `pub`.

---

## 3. Example seam — the prompt component

The operator's ask: header, body and footer as three regions with
host-registered callbacks; vi, emacs and nano; the Esc handler included; and a
component a fork of codex or DeepSeek-TUI could adopt.

The answer that survived adversarial review is a **render-free core**: it owns
the buffer, the editors and the claim accessors, and owns nothing else.

```mermaid
sequenceDiagram
    participant T as terminal
    participant H as host (newt / codex fork)
    participant L as precedence-ladder
    participant P as agent-prompt core

    T->>H: key event (crossterm, egui, whatever)
    H->>P: claims()
    P-->>H: ["vi-insert"] or ["palette"] …
    Note over H: host merges ITS claims<br/>("approval", "spill-explore")
    H->>L: resolve(trigger, situation)
    L-->>H: Escape | Claimed
    H->>P: key(k, Route::Deliver | Route::Escape)
    P-->>H: Effect::{Emit, Submit, Eof, None}
    H->>T: render rows + perform the interrupt
```

**The core never resolves Esc; it only names its claims.** This is the one
seam defect reviewers judged fatal in two of three candidate designs: a core
that resolves internally *and* exposes `claims()` lets a host claimant which
outranks `vi-insert` discover it won only *after* Esc has already left insert
mode. Naming claims and resolving in the host makes that unrepresentable.

Two consequences fall out for free:

- The core takes **no dependency on `precedence-ladder`**, so it does not
  inherit that crate's frozen MSRV 1.88 — which unblocks `monitor-agent` at
  1.80. The risk is deleted rather than mitigated.
- The footer hint becomes a **host closure**. A hint the core derives from a
  claim set it assembled alone is wrong exactly when a host modal is open.

### The ownership boundary, stated

| Concern | Owner |
|---|---|
| terminal, raw mode, event loop, key decoding, the clock, layout | **host** |
| buffer, cursor, undo, kill store, wrap, display width | **core** |
| mode state machines (vi / emacs / nano) as keymap **data** | **core** |
| Esc **claims** | **core** |
| Esc **resolution**, interrupt delivery, press tiering | **host** |
| "does Enter submit?", what `Eof` means | **host** |
| header / footer content, model / endpoint / session vars | **host** (callbacks) |
| rows above the prompt while a turn streams | **host** (`Effect::Emit`) |
| row arbitration under a modal (`RegionLease`) | **host** — deliberately not extracted |

`RegionLease` stays in `newt-core` on purpose: that crate carries `age`,
`ignore`, `regex` and `content-addressable`, and shipping an encryption crate
to someone who wants a vi prompt is not a seam.

### Honest gaps, because a component that overpromises is worse than a small one

Emacs and nano are **not real today**. `Edit::is_modeless()` is true for both,
both route to `tui-textarea`'s default map, and the entire emacs-specific
state in `rich_input.rs`'s 4,618 lines is one `cx_pending: bool`. That
borrowed map is **readline, not emacs**: `C-u` is undo rather than
unix-line-discard, `C-r` is redo rather than reverse-search, `C-w` has the
wrong deletion semantics.

The rule this yields, and it governs the crate:

> **Never bind a canonical key of a named editor to different semantics.**
> nano's `^W` is Where-Is; binding it to delete-word under the name "nano" is
> worse than leaving it unbound, because it destroys text under a key that
> promises search.

Unimplemented canonical keys are **unbound**, enumerated in a per-editor
`KNOWN_GAPS` list whose count may only go down, and excluded from generated
help. Nothing publishes until emacs and nano are real.

Also absent at 0.1: text objects (`ciw`, `da(`) — codex has eight object
types, so a codex fork trades down; search (`/`, `?`, nano `^W`); macros,
registers, visual block. And the core **cannot guarantee Ctrl-C reaches it**:
`cfmakeraw` clears `ISIG`, so a host in cooked mode turns `0x03` into a signal
the core never sees. That is a front-page precondition, not something types
can enforce.

### Getting to "really real" by version bump

Stated by the operator as a requirement, not an aspiration:

> We'll want to MAKE the emacs / nano support "really real" over time in such
> a way that the newt-agent, gilamonster-agent, etc. can just take version
> bumps to get the new features.

That is a compatibility contract, and it is what makes keymaps-as-**data**
load-bearing rather than stylistic. Filling a gap is a data change, so it can
be additive by construction:

| Change | Semver | Why |
|---|---|---|
| a `KNOWN_GAPS` key becomes bound to its **canonical** semantics | **minor** | the key was unbound; nothing that worked stops working |
| a bound key changes semantics | **major** | forbidden in practice by the canonical-key rule above |
| a new editor mode is added | minor | additive |
| a keymap **table** gains a row | minor | data, not API |
| `Effect` or `Key` gains a variant | major unless `#[non_exhaustive]` | so both are `#[non_exhaustive]` from 0.1 |

**The hazard this must not create: a newly-bound editor key silently stealing
a host's binding.** A host that bound `^W` for itself while the crate left it
unbound must not lose it when we implement Where-Is. So the precedence is
fixed and stated:

> **A host binding always outranks an editor binding, in every version.**
> The crate consults the host's map first and only then its own table.

With that rule, filling a gap can never take a key away from a consumer — it
can only light up a key nobody was using. A version bump is then genuinely
safe, which is the operator's requirement discharged mechanically rather than
by care.

**The gap list is the changelog.** `KNOWN_GAPS` is machine-readable, its count
may only go down, and each release's notes are the diff — so
`gilamonster-agent` learns what a bump bought it by reading a generated table,
not a prose summary. The same list generates the help text, so help cannot
advertise a key the version does not implement.

---

## 4. Example seam — dialogs, and why this one ships first

`newt-interaction` is already severable: no `ratatui`, no `crossterm`, no
`newt-*` dependency, with `tests/guard.rs` walking the resolved dependency
closure to prove it — each half carrying an anti-vacuous twin. Identity is a
`ContentId` over canonical DAG-CBOR. It ships published JSON Schemas and a
656-line stdlib-only Python conformance consumer with golden vectors.

```mermaid
graph LR
    D["InteractionDefinition<br/>markdown + controls + roles"]
    D --> P["plain rows<br/>(scroller, headless)"]
    D --> R["ratatui mapper<br/>219 lines"]
    D --> W["newt-web card"]
    D --> X["a codex fork's<br/>BottomPaneView"]

    classDef fork stroke-dasharray: 5 4;
    class X fork
```

Against the Agent SDK's settled vocabulary there are four gaps each way — we
lack `header`, `multiSelect`, per-option `description` and the automatic
"Other" escape; the SDK lacks our `role`, `key`, `aliases` and `Requirement`.
That is a widening job, not a rewrite. And the interactive renderer for
`canUseTool` / `allow|deny|ask` is **unclaimed work**: no harness in the
Anthropic cookbook has one.

**Why it crosses forks when a widget crate cannot:** codex pins a *forked*
ratatui, DeepSeek-TUI is on 0.30, newt and monitor-agent are on 0.29. Any API
naming a ratatui type is dead on arrival across that gap. This one returns
plain data.

---

## 5. The two layers still being designed

**The context manager.** Roughly 8,500 lines across `compress.rs` (5,696),
`prune.rs` (1,414), `content_spill.rs` (876) and `digest_fold.rs` (483) — a
three-stage pipeline whose spans are content-addressed and retrievable by
handle. The operator's ask is that it become *"its own component ... pluggable
... something we can configure and change based on our needs."* The open
questions are the plugin boundary, the authority confinement (a context
strategy decides what the model sees — it is a capability, not a formatter),
and whether it can host #1766's typed operations rather than foreclose them.
**Design in flight; this document will be amended.**

**The theme layer.** The missing stylesheet. Semantic tokens rather than
literals, degradation from truecolor through 256 and 16 to `NO_COLOR`, and a
glyph budget that respects the three fill glyphs verified present in the
`Uni2-Terminus16` PSF console font — because air-gapped lab machines are
exactly where a real console lives. **Design in flight; this document will be
amended.**

---

## 6. What is deliberately NOT extracted

| Candidate | Verdict | Reason |
|---|---|---|
| markdown **renderers** | leave | five live crates upstream; five impls here across three `pulldown` majors |
| focus management | leave | newt has no focus model; the reusable half already shipped as `precedence-ladder` |
| settings form | fold into dialogs | its 1,000 lines are newt's own knob vocabulary; the *pattern* is `InteractionDefinition` |
| monty chart widgets | leave, delete locally | `ratatui` ships `Sparkline`/`Gauge`/`Scrollbar`/`Tabs`; ours are lower-fidelity copies |
| the butterfly meter | offer upstream | genuinely novel, but the audience is at `tui-widgets`, not here |
| `config_panel` / `backend_panel` | leave | 3,589 lines of unconverged legacy already slated for deletion |

---

## 7. Audience, stated without inflation

Measured, not assumed: the Claw repos (NemoClaw, openclaw, MiMo-Code) are
TypeScript with zero Rust. `wyvern-agent` is 713 lines and charter-bound to
strip the TUI. `drake-agent` is 67 lines. `gilamonster-agent` is already
downstream by git rev, so it inherits rather than adopts.

**Real Rust adopters: `monitor-agent`, a `codex` fork, a `DeepSeek-TUI` fork.
Three.** The TypeScript repos can consume the **wire format** — the JSON
Schemas and conformance vectors — even though they can never consume the
crate, which is the only thing in this architecture that reaches them.

That thin audience is the strongest argument for shipping **one** thing well
rather than four adequately. It is worth doing only because `newt-interaction`
is severable at close to zero marginal cost: the work is `cargo publish`, a
README, and cutting `precedence-ladder` 0.1.0 — which is already blocking
`cargo publish -p newt-tui` regardless.

---

## 8. Open questions for the operator

1. **Crate names.** `agent-prompt` and `newt-interaction` as-is, or renamed for
   a stranger's search? Recommendation: keep `newt-interaction` (it is
   published-shaped and the name is unclaimed) and take `agent-prompt`.
2. **Does the theme layer gate the prompt component?** A prompt that hardcodes
   colours would immediately violate §1. Recommendation: ship the prompt core
   render-free (it emits rows and spans, never colours), so the sheet lands
   independently without blocking it.
3. **Text objects before or after 0.1?** They decide whether a codex fork
   adopts. Recommendation: after — but say so on the front page.
4. **Does `newt-tui` publish at all**, or only export? Recommendation: export
   first; publishing is blocked on `precedence-ladder` 0.1.0 and buys little
   until a second consumer exists.
5. **Where does the phone browser live** — this line, or someone else's?
   Recommendation: defer until the sheet exists; it is the layer that makes a
   third browser cheap.
