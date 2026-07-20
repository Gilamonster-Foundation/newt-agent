# Decision: one reusable TTY widget suite, under the line arbiter

**Status:** Proposed (2026-07-19)
**Date:** 2026-07-19
**Builds on:** `newt-core/src/tty/` (the line arbiter, landed) —
`arbiter.rs`, `caps.rs`, `frames.rs`, `spinner.rs`, `mod.rs`
**Governed by:** `docs/decisions/plain_scroller_tui.md` (Accepted 2026-06-12,
amended 06-17 / 06-20, partially superseded 07-16) — this doc adds **no**
carve-out to that list and is subordinate to it.
**Related:** newt-agent#1312 (the reusable TUI widget suite), PR #1314 (the
line arbiter this builds on), `docs/decisions/live_spill_viewport.md` (the
multi-row surface the suite deliberately does not become).

**Method rule:** every widget below is an *adaptation of an existing seam* in
`newt-core::tty` or an existing pure formatter. Nothing in this doc creates a
second drawing path beside the arbiter.

**Survey caveat:** §1–§5 are a measurement of the tree as of 2026-07-19. Line
numbers drift; where implementation has since proven a claim wrong, the
correction is recorded inline as a **[CORRECTION]** note rather than by
silently editing the survey.

---

## 0. The base — what the arbiter already settled, and the one gap it left

The arbiter owns four things and stops:

| Primitive | Site | Role |
|---|---|---|
| `LineLease::paint` | `newt-core/src/tty/arbiter.rs:185` | the ONE ephemeral row; no-op while a `PromptWindow` is alive |
| `LineLease::emit_line` | `newt-core/src/tty/arbiter.rs:201` | the ONE way to commit a permanent line without losing the ephemeral row |
| `LineLease::erase` | `newt-core/src/tty/arbiter.rs:170` | the ONE erase strategy |
| `PromptWindow::{ask, read_line, read_line_into, notice}` | `arbiter.rs:474`, `:482`, `:493`, `:500` | the ONE sanctioned question / blocking read / notice-while-suspended |

Gate: `LineCaps::can_own` (`newt-core/src/tty/caps.rs:30`) consulted through `Terminal::lease` / `lease_with_caps` (`arbiter.rs:257`, `:267`), with `protocol_mode()` (`caps.rs:48`, `:53`) as an irreversible veto at `arbiter.rs:270`. Width: `fit_line` / `FittedLine` (`newt-core/src/tty/mod.rs:94`), today used by exactly one caller (`newt-core/src/tty/spinner.rs:71`). Frames: `frames::SPINNER_FRAMES` (`newt-core/src/tty/frames.rs:13`). Sink is deliberately undefaulted (`arbiter.rs:42`).

**The gap #1312 addresses:** `ask` takes a `&str`. The arbiter has no opinion about what a question, a progress readout, a notice, or an aligned row *looks like* — so every call site invented one. That is the whole of the measured duplication below. The suite supplies **values that render to strings**, and routes every byte through the four primitives above.

---

## 1. INVENTORY — every bespoke widget surface

### Family A — prompts, confirms, menus (12 sites)

| Concern | Distinct impls | Sites |
|---|---|---|
| yes/no parsing | **5** | `newt-tui/src/setup.rs:485`, `newt-tui/src/crew_form.rs:267`, `newt-cli/src/mcp_probe_cmd.rs:401`, `newt-cli/src/dgx.rs:638` (inline), `newt-core/src/agentic/tools.rs:1395` (inline) |
| `Console` trait + `StdinConsole` | **2** (verbatim fork) | `newt-tui/src/setup.rs:36-63`, `newt-tui/src/crew_form.rs:23-48` |
| numbered-menu render + parse | **4** | `newt-tui/src/setup.rs:316`, `:370`, `:441`; `newt-cli/src/dgx_card.rs:288` (`render_menu`) |
| lettered-choice render + parse | **2** | `newt-tui/src/permissions.rs:216-231` + parser at `:121-131`; `newt-cli/src/dgx.rs:2451` + `reconcile_action` at `:2377-2397` |
| default-marking convention | **5** | `[d]eny (default)`, `[Y/n]`, `[y/N]`, `Choose [1]:`, `[a]/[e]/[c]` |
| prompt sigil | **4** | trailing `> `, `\n> `, `: `, `? ` |
| retry-on-invalid | **1 of 12** | only `newt-tui/src/setup.rs:362` (non-empty host loop) |
| EOF behaviour | **4** | empty-answer default (setup, crew_form), `Some("")` (`permissions.rs:302-313`), `Deny` (`permissions.rs:286-291`), `Report`/bail (`dgx.rs:2456`, `dgx_card.rs:281`) |

Two structural defects worth naming, because the suite exists to make them unrepresentable:

1. **Menu literal and parser are unrelated values.** `newt-tui/src/permissions.rs:216-231` hand-types the options; `permissions.rs:121-131` independently lists the accepted keys, ~110 lines away. `[k]ey allow` is advertised with no parser arm; `"A"` is accepted unconditionally by the parser while `[A]llow permanently` is offered only in the Net arm, so `A` at a high-danger exec prompt parses as `AllowPermanent` and is only caught downstream.
2. **The fork diverged behaviourally.** `setup::is_yes` (`setup.rs:485-492`) maps `""` → default and everything unrecognized → `false`; `crew_form::is_yes` (`crew_form.rs:267-274`) maps anything unrecognized → default. `is_yes("maybe", true)` is `false` in setup and `true` in crew_form. Same name, same doc intent, different semantics, produced purely by copy-paste.

**Arbiter coverage today.** Exactly two call paths hold a `PromptWindow`: `newt-tui/src/permissions.rs:677` (`Terminal::suspend_for_prompt()` → `prompt_permission_choice`; the comment at `:667` says *"The ONE place in the workspace that constructs a `PromptWindow`"*) and `permissions.rs:848` (`PermissionGate::ask_question` → `prompt_user_input`, which is what pulls `newt-core/src/agentic/tools.rs:3815` and `:3928` under the guarantee without either site knowing). Six sites block on a human with **no window at all**: `setup.rs:52`, `crew_form.rs:38`, `mcp_probe_cmd.rs:432`, `dgx.rs:637`, `dgx.rs:2456`, `dgx_card.rs:281`. None is a *present* bug (no spinner is live on those subcommand paths) — all six are latent reproductions of the exact defect the arbiter was built to eliminate, and `newt mcp probe` and `newt card pick` both do network work that wants a spinner.

### Family B — progress readouts (4 forks of one loop)

| Fork | Site | Defect |
|---|---|---|
| A `download_to` | `newt-cli/src/models_cmd.rs:213-229` | hand-rolled `\r`; **erase-by-trailing-spaces** (`"   "`); no lease; **no `LineCaps` gate — it draws into a pipe**; no `fit_line`; 5%-delta throttle; bespoke MB math; `eprintln!()` as teardown instead of `Drop` |
| B `download_stream` | `newt-cli/src/models_cmd.rs:335-370` | same loop, **per-MB** throttle; but ships `SetupEvent::Progress { done, total }` over mpsc — the right shape |
| C `setup_status_line` | `newt-tui/src/setup_tui.rs:72-83` | 3rd copy of byte/percent math — but **pure and unit-tested** (`setup_tui.rs:237-247`); the seed |
| D `run_setup_inline` | `newt-tui/src/setup_tui.rs:200-228` | 4th spinner + a **space-pad erase** at `:223`; own `sleep(100ms)` clock at `:226` |

Duplicated arithmetic: `/ 1_048_576` at `models_cmd.rs:223`, `:224`, `:365`, `:366`, `setup_tui.rs:73` (**3 independent copies**, all MB-only, all truncating). Percent: `got * 100 / t` (`models_cmd.rs:221`), `done.saturating_mul(100) / t` (`setup_tui.rs:78`), `used as u64 * 100 / budget as u64` (`newt-core/src/agentic/display.rs:243`) — **only one of the three is overflow-safe**.

### Family C — notices / dim / status writers (~17 sites, one 12-line skeleton)

Sanctioned: `PromptWindow::notice` (`arbiter.rs:500`) and `SpinnerState::emit_detail_line` (`newt-core/src/tty/spinner.rs:117`). Everything else is an `if color { execute!(io::stdout(), …) } else { println!(…) }` twin:

`newt-core/src/agentic/display.rs:66` `print_newt`, `:76` `newt_line`, `:130` `print_harness_notice`, `:148` `print_debug`, `:164` `print_trace`, `:257` `emit_overflow_notice`, `:323` `emit_compression_notice`, `:354` `print_retry_indicator`; `newt-core/src/agentic/warmup.rs:52` (inline, duplicating `Rgb{200,140,0}`), `:44`/`:112`/`:119`; `newt-tui/src/lib.rs:1460` `summarizer_progress`, `:2096` profile line, `:6469` `print_metrics`, `:6485` `print_thinking`, `:6503` `erase_line`; `newt-tui/src/setup_tui.rs:160`; `newt-tui/src/chat.rs:3603`.

Two of these are live hazards, not just duplication:

- **`summarizer_progress` (`newt-tui/src/lib.rs:1445-1470`) is `emit_line` reimplemented from outside the arbiter, and it is a live race.** Its doc comment describes `LineLease::emit_line`'s contract exactly, but it writes raw `\r\x1b[K` to stdout with **no lease** (so `LineLease.painted` is never cleared and the next 100 ms tick fires `Clear(UntilNewLine)` on the row the notice just moved to) and it **cannot see `suspended()`** (`paint` no-ops under a `PromptWindow` at `arbiter.rs:185`; `summarizer_progress` will print over a question). Its gate is `opts.color` at all three call sites (`lib.rs:1610`, `:1655`, `:1667`) — `color` overloaded from styling into I/O ownership. The spinner it claims to cooperate with is already on the arbiter (`newt-core/src/agentic/mod.rs:1508` `tty::with_spinner(…, "compressing context…", Sink::Stdout, …)`); `newt-tui` simply has no handle to the lease.
- **`print_thinking` (`newt-tui/src/lib.rs:6485`) + `erase_line` (`:6503`) are a matched 5th spinner** — a hand-drawn frame-0 (`"⠋ thinking…"`) and the **last open-coded `\r\x1b[K`** in the workspace, both in the chat path, both bypassing the lease.

Amber exists in **4 encodings**: `CtColor::DarkYellow` (`display.rs:135`, `:274`, `:336`), `Rgb{200,140,0}` (`display.rs:360`, `warmup.rs:58`), raw `\x1b[33m` (`lib.rs:1465`). `DarkGrey` + `ResetColor` is open-coded ~23×.

### Family D — aligned columns / rows (9 grid sites + 12 key-value sites)

| # | Site | Width logic |
|---|---|---|
| 1 | `newt-cli/src/mcp_cmd.rs:600` | only site measuring data: `.map(\|r\| r.name.len())` — **bytes**, so non-ASCII names misalign; formats at `:608`, `:619` |
| 2 | `newt-tui/src/probe.rs:1477` | `.len()` again (`:1479`); header `:1487`; **hand-typed dash rule** `:1490` whose dashes must be counted by hand; pads inside cells `:1505`; literal-space placeholders `:1496`, `:1506`, `:1510`, `:1513-1515` |
| 3 | `newt-cli/src/models_cmd.rs:95` | header `{:>6}` vs body `{:>5.1}` + literal `G` (`:105`) — aligned only by coincidence |
| 4 | `newt-cli/src/tuning_cmd.rs:131` | `"─".repeat(68)` at `:134` — magic 68 re-derived by hand |
| 5 | `newt-cli/src/dgx_card.rs:97` + `:112` | format string literally duplicated 15 lines apart |
| 6 | `newt-cli/src/dgx_card.rs:301` | numbered variant of #5 with a different width (`{:>7}` vs `{:>6}`) |
| 7 | `newt-cli/src/dgx_status.rs:228` | `"    {:<7} {:>6.1} GiB  {}"` |
| 8 | `newt-cli/src/ocap_cmd.rs:220` | literal 8-space continuation indent |
| 9 | `newt-tui/src/lib.rs:997` | same shape as #8, different constants |

Key/value padding, all hardcoded: `newt-tui/src/lib.rs:2151`, `:2154`, `:2163`, `:2179`, `:2181` (`{label:<9}` **6× in one function**), `lib.rs:176`, `newt-tui/src/chat.rs:1761`, `newt-tui/src/lib.rs:4203`, `:4254`. And `newt-cli/src/doctor.rs` — **~30 bare `println!`** with hand-typed indents (`:16`, `:24`, `:36`, `:50`, `:82`, `:171`, `:184`, …): the section+indented-row shape with no columns at all.

**Four competing width models, none used by any of the 21 sites above:**

1. `newt_core::tty::fit_line` (`newt-core/src/tty/mod.rs:94`) — **char count**, public, 1 caller.
2. `display::wrap_to_width` (`newt-core/src/agentic/display.rs:29`) — char count, multi-line; `pub(crate)`, invisible above `newt-core`.
3. `markdown::width::{str_width, ch_width}` (`newt-core/src/agentic/markdown/width.rs:17`, `:12`) — **the correct one**, real `unicode-width`; `pub(super)`, locked in the markdown module.
4. `newt-tui/src/spill_view.rs:665` `char_width` — hand-rolled, **hardcoded glyph allowlist**, else "conservatively budget two cells".

Plus a fifth wrapper at `newt-core/src/agentic/transcript.rs:127` duplicating `wrap_to_width`.

**The table renderer already exists and is merely private.** `newt-core/src/agentic/markdown/table.rs` has every piece the nine sites hand-roll: natural widths from data (`:155`), correct measurement via `ch_width` (`:37`), per-column alignment incl. centering (`:80-84`), `…` truncation (`:48`), budget fitting that shaves the widest column above a 1-col floor (`shrink`, `:111-131`), overhead derivation `let overhead = 3 * ncols + 1;` (`:161` — precisely what `tuning_cmd.rs:134`'s magic `68` does by hand), and generated separator rules (`:168` — `probe.rs:1490`'s hand-counted dashes done correctly). It is `pub(super)` at `:17` and `:136`.

### The stated cause, already written down in the repo

`newt-core/src/tty/frames.rs:3-9`: the spinner frames were duplicated across three crates *"not by design, but because `agentic::display` was a private module with a curated re-export list, so nothing outside `agentic` could import them."* `newt-core/src/agentic/display.rs:15-17` repeats it: *"that privacy was the mechanical cause of the duplicate frame sets and open-coded erase escapes elsewhere."*

**That cause is still live, and `frames.rs:3-9`'s de-duplication claim is currently false.** It names `newt-tui/src/setup_tui.rs` as de-duplicated; `newt-tui/src/setup_tui.rs:54` still reads `const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];` — a verbatim copy of `frames.rs:13`, with its own `frame % len` clock at `:140` and `:213`. Copy #6 survived the consolidation. `newt-core/src/agentic/mod.rs:23` is still `mod display;` with a curated `pub use` at `:156-159` exporting 10 names, while `display.rs` holds this entire widget family.

---

## 2. SCOPE BOUNDARY

### The governing text, quoted

`docs/decisions/plain_scroller_tui.md`, Decision rule 2:

> **No advanced TUI in newt.** The following do not get added to the chat path: alternate screen, raw-mode UI loops, scroll regions, panes/splits, persistent status bars, full-screen widget frameworks (ratatui, cursive, …), mouse handling, or live-updating dashboards. Multi-line redraws are limited to the standing carve-outs below.

Rule 3:

> **Feature pressure gets redirected, not absorbed.** Wants for richer UI belong in **gilamonster-agent** … or the **monitor-agent** repos. If a newt change seems to need a richer surface, that is the signal the feature belongs in those repos — not that newt should grow the surface.

Rule 4:

> **Strippability is a requirement.** wyvern-agent is a stripped-down build of this same agent design. Anything added to newt's interactive surface must be cleanly severable so the headless core stays light.

The carve-out list header:

> ### Standing carve-outs (the full list — additions need a new decision)

The two carve-outs the suite may render *into*, but not *become*:

> **Single-line, same-line indicators** — e.g. the `▸ thinking…` indicator erased with `\r` (`print_thinking` / `erase_line`). One line, erased in place, never a region.

> **ANSI color and column escapes in scrolled output** (header, prompts, diff coloring), always behind `color_supported` degradation.

And the constraint that most directly bounds Family D, from the Context section:

> **Small surface, small deps.** The committed scroller has no general layout engine; the narrowly bounded TTY surfaces own their resize/redraw tests.

Plus the rejected-alternative note that forecloses any "status region" ambition:

> > **Rejected: a pinned idle status bar.** A version that pinned the status to the bottom rows via a DECSTBM **scroll region** was prototyped and **backed out** … The "no scroll regions / no persistent status bars" rule above **stands**.

### What the suite MAY contain

- Pure formatters returning `String` / `Vec<String>`, computed from data and a column budget.
- Exactly **one** ephemeral row, painted through `LineLease::paint`, erased through `LineLease::erase` — the existing `Spinner` shape, nothing more.
- Permanent lines committed through `LineLease::emit_line`, or plain `println!` fallback when no lease exists.
- Human-blocking interaction only through `PromptWindow::{ask, read_line, read_line_into, notice}`.
- ANSI color only behind `LineCaps` / `color_supported` degradation.

### What the suite MUST NOT contain

- Any ratatui / cursive dependency, or `EnterAlternateScreen`. Note the mechanical enforcement: `ratatui.workspace = true` is non-optional in `newt-tui/Cargo.toml:38` and **absent from `newt-core/Cargo.toml` entirely**. Placing the suite in `newt-core` makes this rule unbreakable rather than merely reviewed. (This is an independent argument for §4's placement.)
- Any `MoveTo` / absolute row addressing in a public API. A "progress region" or "multi-row progress" would let `run_setup_screen`'s geometry migrate toward the chat path inside a blessed abstraction.
- Any redraw / re-render method on a rendered value. A `Question` that renders to a `String` and is printed once is a *formatter*; a `Question` that can repaint itself with a highlighted row requires raw mode and **is** the violation. **The word "widget" is the trap in #1312's own framing.**
- A general layout engine, per the quote above. The table widget is a **column-width computation producing lines**, not boxes, panes, or nesting.
- Persistent status bars, scroll regions, mouse handling, live dashboards.

### Things that cross the line — and how each is handled

| Item | Verdict | Handling |
|---|---|---|
| `run_setup_screen` (`newt-tui/src/setup_tui.rs:117-167`) — alt screen, `Clear(ClearType::All)` + `MoveTo(0,0)` at `:135`, `MoveTo(2,row)` at `:155-159`, footer at `:161-167` | **Out of scope. Not touched, not wrapped, not offered a component.** | It rides in under the *startup splash* carve-out, which is closed (*"the full list — additions need a new decision"*). A "wizard screen" widget with title/body/footer/selection **is** the general layout engine the doc says the scroller does not have; building it would let the next `MoveTo` screen land inside a blessed abstraction rather than being caught in review. Leave it exactly as it is. |
| `run_setup_inline` (`setup_tui.rs:204-228`) — its plain-scroller sibling | **In scope, pure win.** | It is a single ephemeral status line. Becomes `Progress` on a lease. Deletes `setup_tui.rs:54`'s `SPINNER` copy, the `sleep(100ms)` clock at `:226`, and the space-pad erase at `:223`. No scope risk: nothing it does needs geometry. |
| `newt-tui/src/rich_input.rs:402-409` — in-line `[y/N]` confirm rendered inside the raw-mode input row (`Some(Confirm::SubmitQuit) => Some("send prompt then quit? [y/N] ")`), parsed at `newt-tui/src/vi.rs:180` | **Explicitly NOT unified. A 7th yes/no convention that stays a 7th.** | It lives on the severable `rich-tui` surface sanctioned by the #416 two-mode amendment, not the committed scroller. Pulling it into the suite is the single move that would genuinely smuggle rich-TUI concerns into a shared plain-scroller primitive — and it would couple `newt-core` to a `rich-tui`-gated surface, breaking rule 4. Recorded here so nobody "finishes the job" later. |
| `newt-core/src/agentic/display.rs:816` `GaugeLevel` — documented as color-type-agnostic *"so each caller maps it to its own palette (crossterm for the scroller, ratatui for the rich header)"* | **Precedent to follow in reverse.** | A ratatui consumer of a newt-core formatting type is already a design assumption. The suite must stay pure-string + `LineLease` and expose **no ratatui-shaped surface**, so nothing in it becomes the seam through which a rich header enters the chat path. |
| `newt-tui/src/spill_view.rs:665` `char_width` | **Callsite fix, not a suite member.** | Point it at the promoted `tty::width::ch_width` and delete the glyph allowlist. The live-spill viewport itself stays under `docs/decisions/live_spill_viewport.md` and the `live-spill` feature; multi-row is the viewport's province (`Ephemeral` at `arbiter.rs:87-94` notes the viewport joins later), never the suite's. |

**The safe framing, in one sentence: de-duplicate the formatters that produce the lines; leave the surfaces alone.**

---

## 3. THE SUITE

Four widgets and one primitive promotion. Each absorbs a measured duplication count from §1; none is speculative.

### 3.0 Promotion: `newt_core::tty::width` (not a widget — the missing primitive)

Move `markdown::width::{str_width, ch_width}` (`newt-core/src/agentic/markdown/width.rs:12`, `:17`) up to `newt-core/src/tty/width.rs`, `pub`, and re-export into the markdown module so `markdown/table.rs` keeps compiling unchanged. Add `wrap_line(s, cols) -> Vec<String>` by moving `display::wrap_to_width` (`display.rs:29`) and pointing `transcript.rs:127` at it.

> **[CORRECTION] (step 1, implemented 2026-07-19)** — two survey errors:
>
> 1. **`unicode-width` is an OPTIONAL dependency gated on the `markdown`
>    feature** (`newt-core/Cargo.toml:77`, `:132`). The survey missed this. Since
>    `tty` is unconditional and step 2 makes `fit_line` measure through
>    `str_width`, the dependency **must become non-optional** — done in step 1,
>    with `markdown` narrowed to `["dep:pulldown-cmark"]`. It is a pure
>    lookup-table crate with no transitive dependencies, so the wyvern strip
>    pays essentially nothing.
> 2. **`transcript.rs`'s wrapper is NOT a duplicate of `display::wrap_to_width`,
>    and must not be pointed at `wrap_line`.** The two are different algorithms:
>    `display::wrap_to_width` splits on `split_inclusive(' ')` and therefore
>    **keeps the break space** as a trailing space on each wrapped chunk, while
>    `transcript.rs:178` breaks at the last space before the limit and **drops
>    it**. Measured, `"hello there friend"` at width 10 gives
>    `["hello ", "there ", "friend"]` vs `["hello", "there", "friend"]` — a
>    visible text change in the downstream cowork pane, and not an enumerated
>    diff in §5. `transcript.rs`'s own tests do **not** catch it (they pass
>    against either implementation), so green tests were not evidence here.
>    Unifying the two is a real behavior decision and belongs in its own step
>    with its own goldens, not inside the "pure move" step.

```rust
pub fn ch_width(c: char) -> usize;
pub fn str_width(s: &str) -> usize;      // display columns, unicode-width
pub fn wrap_line(s: &str, cols: usize) -> Vec<String>;
pub fn fit_line(s: &str, max_cols: usize) -> FittedLine;  // exists, tty/mod.rs:94
```

`fit_line` stays **the single width primitive for one-line fitting**; every widget below fits through it and measures through `str_width`. Kills: 4 competing width models → 1; 2 byte-vs-column bugs (`mcp_cmd.rs:602`, `probe.rs:1479`); 1 hardcoded glyph allowlist (`spill_view.rs:665`); 1 duplicate wrapper (`transcript.rs:127`).

`fit_line`'s own char-count implementation should switch to `str_width` internally — a behaviour change only for non-ASCII spinner labels, and a bug fix. Flagged in §6 as its own test.

### 3.1 `Question` — the one prompt widget

Adapted from `permission_prompt_text` (`newt-tui/src/permissions.rs:169-237`), the only composed prompt in the workspace. **Does not invent a trait.**

```rust
pub struct Question<'a> {
    pub header:  &'a str,                  // "⊘ bash wants to run `rm -rf /` — outside …"
    pub context: Vec<Cow<'a, str>>,        // blast radius, reason, aside — each rendered on its own indented line
    pub options: Vec<Opt<'a>>,             // EMPTY = free-text
    pub default: Option<usize>,            // index into options; None = no default
    pub retry:   RetryPolicy,
}

pub struct Opt<'a> {
    pub keys:  &'a [&'a str],   // ["a"] | ["y","yes"] | ["1"] | ["e","enforce"]
    pub label: &'a str,         // "allow once"
    pub note:  Option<&'a str>, // "(high-danger: [s]ession allow refused …)"
}

pub enum Style { Bracketed, Numbered, YesNo }   // controls RENDERING ONLY
pub enum RetryPolicy { FailToDefault, RetryUpTo(u8) }

impl Question<'_> {
    pub fn confirm(header: &str, default_yes: bool) -> Self;      // absorbs all 5 yes/no parsers
    pub fn choice(header: &str, options: Vec<Opt<'_>>) -> Self;
    pub fn numbered(header: &str, labels: &[&str]) -> Self;       // Opt{keys:["1"]} generated by index
    pub fn free_text(header: &str) -> Self;

    pub fn render(&self, cols: usize, style: Style) -> String;    // pure; fits via tty::fit_line
    pub fn parse(&self, input: &str) -> Option<usize>;            // walks the SAME options vec
}

impl PromptWindow {
    /// The natural widening of `ask(&str)` (arbiter.rs:474). Keeps the seal intact.
    pub fn ask_question(&self, q: &Question<'_>) -> Answer;
}

pub enum Answer { Chose(usize), Text(String), Eof, Invalid }
```

**Why this is the whole fix:** `render` and `parse` walk one `options` vec, so the menu literal and the parser cannot disagree. `[k]ey allow` advertised with no arm becomes a compile-time impossibility (it is either an `Opt` or it is not rendered); `"A"` accepted where `[A]` was never offered becomes impossible by construction. That is `permissions.rs:216-231` vs `:121-131` closed structurally, not by discipline.

**`retry` is explicit, never defaulted.** `permissions.rs:116-120`'s fail-closed-to-`Deny` is load-bearing security behaviour and must not be silently converted into a retry loop by a shared helper. Permissions keep `FailToDefault`. `dgx_card.rs`'s pick gets `RetryUpTo(3)` instead of exiting the process on a typo (`parse_selection` at `dgx_card.rs:319-328` already produces the human-readable errors — today they are thrown into `anyhow!` and the process dies).

**Free text keeps its own path.** `Question::free_text` renders header + sigil; the read is `PromptWindow::read_line_into` directly, preserving `interpret_user_line`'s EOF distinction (`permissions.rs:302-313`: `Ok(0)` → `Some("")`, `Err` → `None`). Only the `[y/N]`-suffix convention is unified; the post-filter `is_slash_command_at_prompt` (`permissions.rs:341`) stays at its call site.

**Both `Console` traits die.** `newt-tui/src/setup.rs:36-63` and `newt-tui/src/crew_form.rs:23-48` exist solely for scripted-answer testing, and `PromptWindow::test_stub()` (`arbiter.rs:510`) already serves that need. Adapt the wizards onto the stub and delete both traits and both `StdinConsole`s (~60 lines), which makes the `is_yes` divergence disappear by construction rather than by picking a winner. **Do not introduce a third abstraction.**

Because `ask_question` requires `&PromptWindow`, migrating the six bare-`read_line` sites is free and the latent bug becomes unrepresentable, per CLAUDE.md's *"prefer making a bug unrepresentable over fixing each site."*

### 3.2 `Progress` — the one progress widget (an adapter over `Spinner`, not a new drawing primitive)

```rust
pub struct Progress { /* label, done, total, unit; owns a Spinner */ }

impl Progress {
    pub fn start(caps: LineCaps, label: &str, sink: Sink, color: bool) -> Option<Self>;
    pub fn update(&self, done: u64, total: Option<u64>);   // MUTATES STATE ONLY — writes nothing
    pub fn set_label(&self, label: &str);
    pub fn emit_line(&self, text: &str);                   // permanent, via LineLease::emit_line
}
// Drop erases through the lease.

pub fn progress_line(label: &str, step: &str, done: u64, total: Option<u64>) -> String;
pub fn humanize_bytes(n: u64) -> String;   // KB/MB/GB
```

`progress_line` is `setup_tui.rs:72-83`'s `setup_status_line` **widened, not rewritten** — it is already pure and tested (`setup_tui.rs:237-247`), so its tests carry forward; `mb()` becomes `humanize_bytes`, the fixed prefix becomes a `label` parameter.

Rendering is `Spinner::set_label(&progress_line(...))` (`newt-core/src/tty/spinner.rs:253`); the 100 ms ticker repaints. **The producer never writes bytes and never throttles** — the arbiter's clock already subsumes both byte policies, so the 5%-delta (`models_cmd.rs:220`) and per-MB (`models_cmd.rs:364`) throttles delete outright, as does every erase strategy in the family. The gate is `LineCaps` via `Spinner::start_with_caps`, which fixes the `newt models pull`-into-a-pipe case for free; `Sink` is carried explicitly so `Sink::Stderr` keeps `2>/dev/null` honest (`arbiter.rs:38-40`: *"the setup wizard and the model downloader write progress to stderr, and silently relocating those bytes to stdout would break someone's `2>/dev/null`"*).

`SetupEvent` (`newt-tui/src/setup_tui.rs:31`) is the right producer interface and **stays**; Fork A (`models_cmd.rs:213-229`) folds into Fork B (`:335-370`) with a `Progress` on the receiving end.

**API constraint (guardrail):** no `MoveTo`, no row count, no region. Exactly one ephemeral row + `emit_line` for permanents.

### 3.3 `Notice` — the one notice widget (a value, not a printer)

```rust
pub enum Level { Info, Ok, Warn, Loud, Dim, Debug }

pub struct Notice<'a> { pub level: Level, pub glyph: &'a str, pub text: Cow<'a, str> }

impl Notice<'_> {
    pub fn line(&self) -> String;          // pure, no ANSI — the existing *_msg fns become constructors
    pub fn emit(&self, caps: LineCaps, sink: Sink, color: bool);
}
```

**Routing is the load-bearing part.** `emit` asks the arbiter for the live lease and uses `LineLease::emit_line` (`arbiter.rs:201`); when a `PromptWindow` is live it routes to `PromptWindow::notice` (`arbiter.rs:500`); with neither, plain `println!` after the `LineCaps` gate. That one change makes `summarizer_progress`'s race (`newt-tui/src/lib.rs:1445-1470`) unrepresentable, and makes `newt_line`'s split (`display.rs:76` — which exists *purely* so a `PromptWindow` holder can re-route the same bytes) unnecessary: `PromptWindow::notice` becomes just another routing branch of one function.

`Level` maps to the ONE hue table, collapsing `DarkYellow` / `Rgb{200,140,0}` / `\x1b[33m` into one amber and `DarkGrey` into one dim, and preserving the accessibility rule already documented at `display.rs:60-63` — **the glyph carries the meaning; color is never alone.** (This is also the user-standing requirement that accessibility is a config requirement, not a nicety.)

Absorbs: `print_newt`, `print_harness_notice`, `print_debug`, `print_trace`, `print_retry_indicator`, `emit_overflow_notice`, `emit_compression_notice`, `summarizer_progress`, the `warmup.rs:52` inline block, `print_metrics`, `chat.rs:3603`. Every pure text builder survives as a constructor — `compression_notice_text` (`display.rs:293`), `retry_progress_msg` (`lib.rs:1451`), `fallback_progress_msg`, `failure_progress_msg` — so their existing tests keep passing verbatim (e.g. `newt-tui/src/lib_tests/http_loop.rs:401` asserting `"⚠ summarizer falling back to qwen:0.5b…"`). **The deletion is confined to the ~12-line `if color { execute! } else { println! }` skeleton, ×15.**

`print_thinking` (`lib.rs:6485`) and `erase_line` (`:6503`) are **deleted and replaced by a `Progress` with no total**, not wrapped — wrapping preserves the byte-level fork and the 5th spinner.

### 3.4 `Rows` — the one aligned-output widget

**Promoted, not written.** Make `newt-core/src/agentic/markdown/table.rs`'s machinery `pub` under `newt_core::tty::rows`, widen its input beyond `Cell`, and add a border style.

```rust
pub enum Border { Plain, Ruled, Boxed }   // Plain: space-separated, no box chars, NO SGR
pub enum Align  { Left, Right, Center }

pub struct Rows<'a> {
    pub header: Option<Vec<&'a str>>,
    pub aligns: Vec<Align>,
    pub rows:   Vec<Vec<Cow<'a, str>>>,
    pub indent: usize,
    pub border: Border,
}
impl Rows<'_> { pub fn render(&self, cols: usize) -> Vec<String>; }   // pure

pub fn key_values(pairs: &[(&str, &str)], indent: usize) -> Vec<String>;  // label col auto-sized
pub fn section(title: &str, body: Vec<String>) -> Vec<String>;            // doctor.rs's shape
```

Existing machinery reused as-is: natural widths (`table.rs:155`), `vis_width` (`:37`), alignment incl. centering (`:80-84`), `truncate` (`:48`), `shrink` (`:111-131`), `overhead = 3 * ncols + 1` (`:161`), generated rules (`:168`). Only the `Cell`/`Style` input type (`newt-core/src/agentic/markdown/inline.rs`) needs adaptation.

`Border::Plain` exists specifically so the nine sites emit **byte-identical output** post-migration (see §5). `key_values` subsumes the six `{:<9}` sites in `lib.rs:2151-2181`; `section` gives `doctor.rs` its shape without touching its ~30 `println!`s' text.

**Scope discipline for this one:** `render` returns `Vec<String>` and takes a column budget. It has no nesting, no cells-containing-tables, no borders beyond the three enumerated, and no color. That is a column-width computation, which is what `plain_scroller_tui.md`'s *"no general layout engine"* permits.

### What is deliberately NOT in the suite

- **The alt-screen wizard screen** (§2).
- **`rich_input`'s inline confirm** (§2) — 7th convention, stays 7th.
- **`GaugeLevel` / `fmt_token_gauge`** — already correctly shaped (color-agnostic value, caller maps palette); no duplication measured; leave alone.
- **Retry-as-a-default** — an explicit `RetryPolicy` field, because collapsing fail-closed-to-deny into a shared retry loop is a security regression wearing a refactor's clothes.
- **A `Console` trait in any form.** `PromptWindow` + `test_stub` is the abstraction.

---

## 4. WHERE IT LIVES

**Decision: `newt-core/src/tty/widgets/` — a `pub` module under the existing `newt-core::tty`. Not a new crate.**

### Dependency direction

Measured `[dependencies]` edges: `newt-core` depends internally on only `newt-skills` and `newt-tuner` (`newt-core/Cargo.toml:63`, `:41`), neither of which touches the terminal (`crates/newt-tuner/src/main.rs:77` is a bare `println!` of TOML; `newt-skills/src` has zero `tty` hits). Everything terminal-touching sits above: `newt-tui` (`newt-tui/Cargo.toml:15-21`), `newt-cli`/pkg `newt-agent` (`newt-cli/Cargo.toml:25-34`), `newt-mcp-server`, `newt-mcp-client`, `newt-acp-worker`, `newt-tools`, `newt-inference`, `newt-coder`, `newt-scheduler`, `newt-identity`. The sole non-consumer is `newt-mcp-data` (`newt-mcp-data/Cargo.toml:22`: *"deliberately NO agent-bridle / newt-inference / newt-core / caveats"*), which needs no widgets.

**`newt-core` is therefore the lowest crate that reaches every consumer. There is no lower placement available and none needed.**

### The process-singleton constraint forces the crate

`newt-core/src/tty/arbiter.rs:119-122`:

```rust
fn arbiter() -> &'static (Mutex<Inner>, Condvar) {
    static ARBITER: OnceLock<(Mutex<Inner>, Condvar)> = OnceLock::new();
```

and `newt-core/src/tty/mod.rs:32-35` states the rule outright:

> **`newt-core::config` does not probe the terminal; `newt-core::tty` does, and it is the only place that may.** The arbiter must be a process singleton serving `newt-core::agentic`'s own spinner as well as every crate above it, so `newt-core` is the only crate that can host it.

A widget suite in a *sibling* crate cannot host the singleton, so it must depend on `newt-core` — which **excludes `newt-core::agentic` from using it**, and `agentic` already drives `tty::with_spinner` / `Spinner::start_with_caps` at `newt-core/src/agentic/mod.rs:1508`, `:1676`, `:6128`. That is precisely the "duplicate beside it" outcome this design forbids. The only coherent alternative is extracting the whole arbiter into a `newt-tty` crate — far larger and riskier than #1312 warrants, and actively dangerous: a `OnceLock` singleton is safe only while exactly one crate version is linked, and a separately-versioned `newt-tty` invites two semver-incompatible arbiters in one process.

### The many-small-crates preference: acknowledged, and it does not apply here

The standing preference is real, and the usual payoffs are absent in this case:

1. **No dependency isolation to win.** The widgets are pure formatters; `crossterm` is already a non-optional `newt-core` dep (`newt-core/Cargo.toml:59`).
2. **Publishability is already satisfied.** `newt-core` ships to crates.io on the workspace pin (`Cargo.toml:158`, `newt-core/Cargo.toml:39-41`: *"MUST come from the workspace pin (path + `=` version) — a path-only dep here fails `cargo publish` outright and broke the v0.7.2 release"*). A `pub mod tty::widgets` reaches downstream consumers at zero extra release cost, and `plain_scroller_tui.md:93-95` names gilamonster-agent as *"the feature-rich agent matrix that inherits newt's published crates"* — the downstream-reuse motive behind the preference is met by a public module.
3. **A separate crate would weaken the ratatui firewall.** `ratatui` is absent from `newt-core/Cargo.toml` entirely. A `newt-widgets` crate could quietly grow the dep and satisfy CI; a module in `newt-core` cannot. Placement is enforcement.

If a genuinely reusable, newt-independent terminal-widget crate is wanted later, the right extraction is the whole of `newt-core::tty` (arbiter included) in one move, versioned once — not the widgets alone.

### The visibility lesson, applied

`frames.rs:3-9` and `display.rs:15-17` both name **module privacy with a curated re-export list** as the mechanical cause of the sprawl this issue is cleaning up. So:

- `newt-core/src/tty/widgets/` is `pub`, with no curated re-export gate.
- `agentic::display`'s formatters are **moved into it**, not re-exported through `agentic/mod.rs:156-159` a second time.
- `markdown::width` and `markdown::table`'s renderer move up to `tty::` and the markdown module re-exports downward.

### Strippability

The suite must compile and lint clean under `--no-default-features` (CI job `.github/workflows/ci.yml:81` "lean strip-down build", `:96` `cargo clippy -p newt-agent --no-default-features --all-targets -- -D warnings`; mirrored at `justfile:47`, `:127`), i.e. with `markdown`, `rich-tui`, and `live-spill` all off (`newt-cli/Cargo.toml:70`, `newt-core/Cargo.toml:129`).

The `Rows` promotion is the live risk, since its machinery currently sits under the `markdown` feature. **Follow the existing precedent** (`newt-core/Cargo.toml:126-128`: `markdown` is stripped by *"the passthrough shim in `agentic::markdown`"*, a feature-off identity function, not a `#[cfg]` hole at each call site): `tty::rows` becomes **unconditional** (it has no markdown dependency once `Cell` is generalized), and `agentic::markdown` depends *on it*, reversing today's direction. Wyvern gets `Rows` and `Notice` for free; `--no-default-features` builds keep `Rows` because `newt mcp list`'s table exists in the lean build too.

---

## 5. MIGRATION — ordered, independently verifiable

Each step is a separate PR, lands green on its own, and is revertible without the next.

**Verification vocabulary used below:**
- **[GOLD]** — capture the exact stdout/stderr bytes of the command **before** the change into a fixture, assert byte equality after. Non-negotiable on the non-interactive path.
- **[UNIT]** — existing pure-formatter test carried forward unmodified.
- **[PTY]** — real-resource PTY test (§6).

| # | Step | Bespoke site → what it becomes | Proof of no behavior change |
|---|---|---|---|
| **1** | Promote width | `markdown/width.rs:12,17` → `tty::width::{ch_width,str_width}`; `display.rs:29` `wrap_to_width` → `tty::width::wrap_line`; ~~`transcript.rs:127` → calls it~~ (see [CORRECTION] in §3.0 — different algorithm, left alone); `unicode-width` becomes non-optional | Pure move. [UNIT] all existing markdown-width and wrap tests unchanged. New: `str_width("日本") == 4`. No output changes anywhere. |
| **2** | `fit_line` uses `str_width` | `tty/mod.rs:94` internals | [UNIT] existing `fit_line` table tests at `tty/mod.rs:119-158` unchanged (all ASCII). **Behaviour change, deliberate:** CJK spinner labels now fit correctly. New test asserts the old char-count result was wrong. |
| **3** | Land `Notice` | new `tty::widgets::notice`, no call sites migrated | [UNIT] `Notice::line()` reproduces `compression_notice_text` (`display.rs:293`), `retry_progress_msg` (`lib.rs:1451`), `fallback_progress_msg`, `failure_progress_msg` byte-for-byte. Zero production diff. |
| **4** | Kill the summarizer race | `newt-tui/src/lib.rs:1445-1470` `summarizer_progress` → `Notice{Warn}.emit(...)`; the three `if opts.color` gates at `:1610`, `:1655`, `:1667` → `LineCaps` | [UNIT] `http_loop.rs:401` (`"⚠ summarizer falling back to qwen:0.5b…"`) passes unmodified. [PTY] new: spinner live + notice emitted → no interleaved bytes; `PromptWindow` live + notice emitted → notice does not overwrite the question. **This is the one step that fixes a live bug, so it goes early.** |
| **5** | Migrate the other 14 notice sites | `display.rs:66,130,148,164,257,323,354`; `warmup.rs:44,52,112,119`; `lib.rs:2096,6469`; `chat.rs:3603` → `Notice` | [GOLD] on every command that emits each notice, incl. `NEWT_COLOR=always TERM=dumb`. Amber unification is the one intentional diff: `Rgb{200,140,0}` sites (`display.rs:360`, `warmup.rs:58`) become `DarkYellow`. **Call it out in the PR body and update those goldens deliberately** — do not let it hide inside a bulk diff. |
| **6** | Delete the 5th spinner | `lib.rs:6485` `print_thinking` + `:6503` `erase_line` → `Progress::start(caps,"thinking…",…)`, no total | [PTY] the `▸ thinking…` indicator still appears and is erased in place on a TTY (its carve-out is explicit at `plain_scroller_tui.md`); [GOLD] zero bytes off a TTY. `erase_line` deletion removes the last open-coded `\r\x1b[K`. |
| **7** | Land `Progress`; migrate `run_setup_inline` | `setup_tui.rs:200-228` → `Progress` + `progress_line`; **delete `setup_tui.rs:54` `SPINNER`** and the `sleep(100ms)` at `:226` and the space-pad erase at `:223` | [UNIT] `setup_status_line` tests at `setup_tui.rs:237-247` carry forward against `progress_line` with the label parameterized. [PTY] frames come from `frames::SPINNER_FRAMES`. **Also fix the false claim at `frames.rs:3-9` in the same PR** — it names `setup_tui.rs` as already de-duplicated. |
| **8** | Fold the download forks | `models_cmd.rs:213-229` (Fork A) folds into `:335-370` (Fork B) with `Progress` on the receiving end; `SetupEvent` (`setup_tui.rs:31`) unchanged | [GOLD] **`newt models pull … \| cat` must now emit *zero* progress bytes** — today Fork A draws into a pipe with no gate. This is a deliberate fix; assert the new empty-on-pipe golden explicitly. On a TTY, [PTY] shows one ephemeral row on **stderr** (`Sink::Stderr` preserved — `2>/dev/null` stays honest). |
| **9** | Land `Question`; migrate permissions | `permissions.rs:169-237` → `Question::choice` + `Style::Bracketed`, `RetryPolicy::FailToDefault`; `permissions.rs:121-131` `parse_permission_choice` → `Question::parse` | [GOLD] byte-identical prompt text for every `DenialKind` × danger tier × reason-present/absent combination — this is the highest-stakes golden in the migration. Fail-closed on invalid/EOF preserved (`permissions.rs:286-291`). **New assertion the old code could not make:** `q.parse("A")` is `None` at a high-danger exec prompt, because `[A]` is not in that `options` vec. |
| **10** | `Question::confirm` for the 5 yes/no sites | `setup.rs:485`, `crew_form.rs:267`, `mcp_probe_cmd.rs:401`, `dgx.rs:638`, `tools.rs:1395` | [GOLD] prompt strings. **Two intentional semantic changes, each its own commit + test:** (a) `crew_form`'s `is_yes("maybe", true) == true` becomes `false` (setup's rule wins — unrecognized input should not be read as consent); (b) `tools.rs:1395` currently rejects `"yes"` at `[y/N]` while every other site accepts it — `Question::confirm` accepts it. Both are the fork being resolved; neither may be silent. |
| **11** | Delete both `Console` traits | `setup.rs:36-63`, `crew_form.rs:23-48` → wizards take `&PromptWindow`, tests use `PromptWindow::test_stub()` (`arbiter.rs:510`) | [GOLD] full `newt setup` and `newt crew edit` transcripts driven by scripted answers. ~60 lines deleted. Both `StdinConsole`s gone → `setup.rs:52` and `crew_form.rs:38` leave the bare-`read_line` list. |
| **12** | Migrate the remaining 4 bare reads | `mcp_probe_cmd.rs:432`, `dgx.rs:637`, `dgx.rs:2456` (`Question::choice`, `reconcile_action`'s short+long forms preserved as `Opt.keys`), `dgx_card.rs:281` (`Question::numbered`, **`RetryPolicy::RetryUpTo(3)`** instead of process exit) | [GOLD] each prompt string and each non-TTY bail message (`mcp_probe_cmd.rs:419`, `dgx_card.rs:271` fail-closed behaviour preserved). `dgx_card`'s retry is an intentional improvement — new test that a typo re-prompts rather than exiting. **After this step the bare-`read_line` list is empty; the §6 anti-sprawl guard can be armed.** |
| **13** | Land `Rows` with `Border::Plain` | promote `markdown/table.rs:17,136` to `tty::rows`, reverse the markdown dependency direction | [UNIT] all markdown table tests unchanged. Zero production diff. Verify `--no-default-features` build in this PR. |
| **14** | Migrate the 9 grid sites | `mcp_cmd.rs:600`, `probe.rs:1477`, `models_cmd.rs:95`, `tuning_cmd.rs:131`, `dgx_card.rs:97`, `dgx_card.rs:301`, `dgx_status.rs:228`, `ocap_cmd.rs:220`, `lib.rs:997` | [GOLD] **one PR per site**, byte-identical. Expect exactly two intentional diffs, both bug fixes with their own tests: `mcp_cmd.rs:602` and `probe.rs:1479` currently measure `.len()` (bytes), so a non-ASCII name misaligns today and will align after — add a non-ASCII fixture row to each golden. `tuning_cmd.rs:134`'s magic `68` becomes derived; assert the derived rule is 68 chars for today's columns. |
| **15** | Migrate key/value + sections | `lib.rs:2151,2154,2163,2179,2181` → `key_values`; `lib.rs:176`, `chat.rs:1761`, `lib.rs:4203`, `:4254`; `doctor.rs` (~30 `println!`) → `section` | [GOLD] `newt doctor` full output byte-identical; the `{:<9}` label column must render at 9 when today's longest label is 9 — assert that explicitly, since auto-sizing is what changes if a label is ever added. |
| **16** | Point `spill_view` at real width | `newt-tui/src/spill_view.rs:665` `char_width` → `tty::width::ch_width`; delete the glyph allowlist | [UNIT] every glyph in the old allowlist measures the same; emoji outside it now measure 2 instead of 2-by-accident. Feature-gated `live-spill`, so verify both feature states. |

**Byte-identity on the non-interactive path is the governing acceptance criterion for steps 5, 8, 9, 10, 11, 12, 14, 15.** Where a diff is intentional it is enumerated above; **any diff not on that list is a regression**, not a judgement call.

---

## 6. INVARIANTS + TESTS

### I1 — Non-interactive byte purity

`newt-cli/tests/stdout_purity.rs:95-119` asserts byte *classes*, under a deliberately hostile env (`.env("NEWT_COLOR","always")`, `.env("TERM","dumb")` — `:68-69`):

```rust
let esc = stdout.matches('\u{1b}').count();
assert_eq!(esc, 0, "stdout carries {esc} ANSI escape(s) — a protocol wire must be plain bytes …");
```

plus zero braille in `U+280B..=U+280F`, extended to mouse private modes at `:220-235`.

**Consequence:** no widget emits a single ANSI byte without consulting `LineCaps`. This test already catches a `Rows` that emits box-drawing or color unconditionally — which is exactly why `Border::Plain` is the migration default.
**New tests:** extend `stdout_purity.rs` to cover every migrated command (`newt models`, `newt probe`, `newt mcp list`, `newt tunings`, `newt card list`, `newt doctor`, `newt dgx status`, `newt ocap …`), asserting zero `\u{1b}` and zero box-drawing chars on stdout.

### I2 — Protocol-mode purity

`enter_protocol_mode()` (`newt-core/src/tty/caps.rs:48`) is called at `newt-cli/src/lib.rs:1145`, `:1281`, `newt-mcp-server/src/main.rs:16`. It is an irreversible veto at `arbiter.rs:270`:

```rust
if super::caps::protocol_mode() || !caps.can_own() { return None; }
```

consulted **live** on every `detect()` because the probe is memoized (`caps.rs:93-95`: *"a stale `Own` there is the Windows JSON-RPC corruption bug"*). `newt-core/tests/tty_protocol_mode.rs:44-53` proves even an explicit `LineCaps::Own` override cannot pierce it.

**Consequence — the invariant most at risk, since `display.rs:66` and `:99` currently write straight to stdout:** every widget that draws routes through `Terminal::lease` / `lease_with_caps`, never `println!` / `execute!(io::stdout(), …)`. That is the only path protocol mode can veto.
**New test:** `tty_protocol_mode.rs` gains cases for `Notice::emit`, `Progress::start`, `Rows::render`+emit, and `PromptWindow::ask_question` after `enter_protocol_mode()` — all must produce zero bytes on stdout.

### I3 — Wyvern strip

CI gate `.github/workflows/ci.yml:81` / `:96`, mirrored at `justfile:47`, `:127`.
**New test:** the widgets module compiles and lints clean with `markdown`, `rich-tui`, `live-spill` all off. `Rows` is unconditional (feature-off identity precedent from `newt-core/Cargo.toml:126-128`), and `agentic::markdown` depends on it rather than the reverse. Add a `--no-default-features` build of a small example that constructs each of the four widgets, so the strip breakage surfaces at compile time rather than at release.

### I4 — The sealed `PromptWindow`

`arbiter.rs:447-450`:

```rust
/// A private, sealed ZST. `PromptWindow` holds one, which is what makes the
/// struct unconstructible outside this module *even with struct-literal syntax*
struct Seal;
```

enforced by three trybuild cases (`newt-core/tests/ui/prompt_window_{cannot_be_struct_literaled,has_no_public_constructor,test_stub_is_not_public}.rs`) driven from `newt-core/tests/prompt_window_is_sealed.rs:19`.

**Consequence:** any widget that blocks on a human takes `&PromptWindow` as a **required** parameter and emits via `ask` / `read_line` / `notice`, never `print!`. `display.rs:72-75` already records the reasoning: *"a notice emitted while a question is on screen must go through the arbiter, or it races the very ticker it was meant to be protected from."*
**New trybuild case:** `question_cannot_be_asked_without_window.rs` — a call to `ask_question` with no `&PromptWindow` must fail to compile. This is the seal extended to the widget layer, and it is what makes the six bare-`read_line` sites unwriteable rather than merely fixed.

### I5 — The `test-util` hazard

`newt-core/Cargo.toml:139-141`: *"Enable it in `[dev-dependencies]`, never in `[dependencies]`."* The trap is spelled out at `newt-tui/Cargo.toml:85-91` and `arbiter.rs:236-243`: cargo unifies features across a workspace build, so enabling `test-util` in `newt-tui` would expose `PromptWindow::test_stub` to every crate and **break the compile-fail seal test**.

**Consequence:** if the suite needs a test observable, follow `prompt_windows_constructed()` (`arbiter.rs:242`) — an unconditionally-public, read-only counter that *"can neither forge a window nor widen anything"*. Never add a `test-util`-gated constructor. The `Console`-trait deletion in migration step 11 uses `test_stub` from **dev-dependencies only**.

### I6 — Singleton test serialization

`newt-core/Cargo.toml:154-157`: the arbiter *"is a PROCESS SINGLETON (one terminal line, one stdin), so its ownership/exclusion tests must not run concurrently"*; `#[serial_test::serial(tty_arbiter)]` on every arbiter test (`arbiter.rs:565`, `:581`, `:589`).

**Consequence — and an API constraint, not just a test rule:** widget tests that acquire a lease inherit that lane; **pure formatter tests must not**, or they slow the suite for nothing. This is the strongest argument for the `Question::render` / `progress_line` / `Notice::line` / `Rows::render` shape: pure `-> String`/`Vec<String>`, tested off-lane, with a thin painting wrapper tested on-lane. If a widget cannot be tested without `#[serial]`, it has been designed wrong.

### I7 — Sink is explicit, never defaulted

`arbiter.rs:38-40`. Every widget API carries `Sink`; a widget that hardcodes stdout regresses the wizard and the downloader.
**New test:** a lint-level guard — grep in the hygiene test (below) for `Sink::Stdout` appearing as a default argument inside `tty/widgets/`.

### I8 — The anti-sprawl guard (repo hygiene)

The whole point: **a new bespoke widget must fail CI, not review.** `newt-core/tests/tty_hygiene.rs` (a source-scanning test over the workspace, with a single explicit allowlist file `newt-core/tests/tty_hygiene_allow.txt`, each entry carrying a one-line justification):

| Banned pattern | Allowed only in | Rationale |
|---|---|---|
| `\r\x1b[K` / `Clear(ClearType::UntilNewLine)` | `newt-core/src/tty/arbiter.rs` | one erase strategy; there were 7 |
| `["⠋"` / any `SPINNER`-shaped `[&str; N]` frame array | `newt-core/src/tty/frames.rs` | there were 6 copies |
| `execute!(io::stdout()` / `execute!(io::stderr()` | `newt-core/src/tty/` | protects I2 |
| `io::stdin().read_line` / `stdin().lock().read_line` | `newt-core/src/tty/arbiter.rs` | protects I4; the list is empty after migration step 12 |
| `enable_raw_mode` / `EnterAlternateScreen` | the four carve-out surfaces named in `plain_scroller_tui.md`, by exact path | enforces rule 2 mechanically instead of by PR review — the doc's own consequence section asks reviewers to do this by eye |
| `MoveTo(` | same four paths | forbids the layout engine |
| `.len()` used as a column width (heuristic: `.len()` inside a `max()` chain feeding a `{:width$}`) | none | catches the `mcp_cmd.rs:602` / `probe.rs:1479` bug class |
| `"─".repeat(` with an integer literal | none | catches `tuning_cmd.rs:134`'s magic 68 |
| `ratatui` in `newt-core/Cargo.toml` | none | the firewall §4 relies on |

Each allowlist entry requires a justification comment, so adding one is a visible, arguable diff rather than an invisible one. **This test is the deliverable that makes the other six invariants durable** — without it, #1312 is a cleanup that decays, which is precisely what happened to `frames.rs:3-9`'s claim.

### Real-resource tests, and what each grounds

CLAUDE.md:193-202 requires this: *"a real-resource test is an add-on that proves the gate is measuring reality. When you add one, record in its doc comment which mocked behavior it grounds. A real test that grounds nothing is just a slow test."* CLAUDE.md:206-207 already names the existing PTY test as grounding *"the line arbiter's mocked lease/suspend unit tests."*

Four PTY tests, each with its grounding recorded in its doc comment:

| PTY test | Grounds |
|---|---|
| `pty_notice_under_spinner` — spinner live, `Notice::emit` fires, assert no interleaved/overwritten bytes | Grounds the mocked `Notice::emit` routing test, which asserts only that `emit_line` was *called*. The mock cannot show that `emit_line`'s erase-then-write-then-leave-unpainted sequence actually survives a concurrent 100 ms tick on a real terminal — the exact thing `summarizer_progress` (`lib.rs:1445-1470`) got wrong while its doc comment claimed correctness. |
| `pty_notice_under_prompt_window` — `PromptWindow` live, `Notice::emit` fires, assert the question text is intact on screen | Grounds the mocked suspend test, which asserts `paint()` returns early (`arbiter.rs:185`). The mock cannot prove a *different* writer (the notice path) is also suppressed — the live race in migration step 4. |
| `pty_progress_erases_on_drop` — `Progress` runs, drops, assert the row is blank and the cursor is at column 0 | Grounds the mocked `Drop`-calls-`erase` unit test. The mock cannot show the terminal is actually clean; the four bespoke teardowns (`eprintln!()` at `models_cmd.rs:228`, space-pad at `setup_tui.rs:223`) each "passed" their authors' eyeball check and each left artifacts. |
| `pty_question_render_wraps_at_real_cols` — a `Question` with a long option list on an 80-col and a 40-col PTY | Grounds the mocked `render(cols)` tests, which pass a synthetic `cols`. The mock cannot prove `term_cols()` returns what the widget assumed, nor that `fit_line`'s ellipsis lands where a real terminal wraps — the failure mode `models_cmd.rs`'s ungated progress line exhibits (overflow wraps and strands rows). |

Everything else stays in the mocked tier: `render`/`line`/`progress_line`/`Rows::render` are pure and tested off the `tty_arbiter` serial lane, per I6.

---

## 7. RISKS, and what the survey could not determine

### Risks

1. **The word "widget" invites the violation.** #1312's own vocabulary — "wizard screens", "progress readouts", "menu rendering" — is the exact vocabulary `plain_scroller_tui.md` rule 3 redirects to gilamonster-agent. The highest-density duplication sites (`newt-cli/src/dgx.rs` alone has 109 `println!`/`eprintln!`; `newt-cli/src/doctor.rs` ~30) are also the most tempting to unify into a *repainting screen* rather than a line formatter — a move that would cross the line while looking exactly like consolidation. Mitigations: `Vec<String>` returns only, no redraw method on any value, `MoveTo` banned by the I8 guard, and `newt-core` having no ratatui dep.
2. **Golden-diff fatigue in migration steps 5, 14, 15.** ~24 sites, one PR each. If they are batched, an unintended byte diff hides in the noise. The per-site PR discipline is load-bearing, not ceremony.
3. **Amber unification is a user-visible change.** Three encodings collapse to one; on some terminals `DarkYellow` and `Rgb{200,140,0}` render distinguishably. Given the standing color-vision constraint (deep-saturated colors lose glyph detail), verify the chosen amber against the accessible-defaults rule rather than picking whichever is more common in the source. The glyph-carries-meaning rule at `display.rs:60-63` is the safety net if the hue choice is wrong.
4. **`Rows` is the widget most likely to grow.** Each of the nine sites has a slightly different shape; the pressure to add `Border::Fancy`, nesting, or per-cell color will arrive with the second migration PR. The enum is closed at three variants deliberately; adding a fourth should require the same argument a new carve-out requires.
5. **Reversing the markdown→rows dependency (step 13) touches a feature boundary.** If `Cell`/`Style` generalization proves messier than expected, the fallback is to duplicate the width computation into `tty::rows` and leave `markdown/table.rs` alone — which reintroduces one duplication to avoid a risky refactor. That trade should be made explicitly, not drifted into.
6. **The `is_yes` and `"yes"`-acceptance resolutions are behaviour changes on consent paths.** Step 10 changes what counts as agreement at `crew_form` and at `tools.rs:1395`'s filesystem-mutation confirm. Both changes are in the safer direction (unrecognized input is no longer consent; `"yes"` is no longer silently rejected), but they are consent semantics and warrant explicit sign-off rather than being carried in a refactor PR.

### What the surveys could not determine

- **Whether any of the 21 aligned-output sites has an external consumer parsing its columns.** The goldens prove the bytes do not change today, but they cannot prove nobody scripts against `newt mcp list`. The two byte-vs-column fixes (`mcp_cmd.rs:602`, `probe.rs:1479`) *will* change output for non-ASCII data. **Unresolved: whether these commands have a documented stable-output promise, and whether a `--json` path exists as the sanctioned machine interface.** Check before step 14.
- **Whether `fit_line`'s char-count → display-width change (step 2) affects any currently-shipping label.** All existing tests are ASCII. Whether any real spinner or progress label carries CJK/emoji today was not established.
- **Whether the `[k]ey allow` option at `permissions.rs:216-231` is intended to remain advertised-but-unbuilt.** The `Question` model makes it either an `Opt` (and therefore parseable) or absent (and therefore not rendered). Rendering it as an unparseable aside is expressible via `Opt.note`, but which the author intends — a real future option, or documentation text — was not determinable from the source. **Needs a decision before step 9.**
- **Whether `newt-cli/src/dgx.rs`'s ~109 print sites contain further table/prompt shapes beyond the ones inventoried.** The surveys sampled it; a full pass was not done. The I8 hygiene guard will surface any that were missed, but the migration estimate for that file is not grounded.
- **Whether `setup_tui.rs`'s alt-screen splash and the `/plan` editor share formatting logic that would benefit from `Rows`.** They may render *using* the suite (§2 permits rendering into carve-out surfaces), but whether they currently duplicate any of it was out of the surveys' scope and was not measured.
- **Whether `run_setup_screen` still needs the alt screen at all.** It has an inline sibling (`run_setup_inline`) that does the same job in the scroller. Retiring the alt-screen variant would shrink a carve-out — strictly a win under `plain_scroller_tui.md` — but that is a separate decision, deliberately not bundled into #1312.