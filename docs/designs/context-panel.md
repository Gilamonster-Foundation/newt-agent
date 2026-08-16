# Context Panel — design spec

Bare `/context` on a rich interactive terminal opens a transient ratatui
`Viewport::Inline` overlay — the `/backend` chooser (#1667) pattern applied to
context management. It follows the house panel grammar (#1665) and the
transaction discipline proven in `backend_panel.rs` and `config_panel.rs`.

Three ideas the operator asked this panel to explore, and how each lands:

1. **Group by availability** — rows are bucketed by whether the thing they
   control actually works *right now*, so "on" never implies "running".
2. **Scale with a probe** — the panel *surfaces* the existing per-model probe
   cache (`probe.rs` → `~/.newt/model-capabilities.json`) and its derived
   send-budget; it does **not** fire network probes itself.
3. **Lighter variant** — the overlay shows the live in-session state; a footer
   points to `/context stats` (the existing pager) for the full per-section
   breakdown, which stays in the pager.

The unifying principle, copied verbatim from `backend_panel.rs`: **the panel is
an editor, never a second authority.** It mutates state only through the same
code paths the text commands already use.

---

## 1. What `/context` actually controls (the nouns)

`handle_context_command` (lib.rs ~9784) mutates a `ContextCommandResult` with
four session-override slots. These are the panel's entire write surface —
nothing more:

| Slot | Set by | Type | Reset |
|------|--------|------|-------|
| `set_manager` | `/context manager <preset>` | `ContextManager` | n/a (only `standard` available) |
| `set_feature` | `/context feature <name> on\|off` | `(ContextFeature, bool)` | per-feature toggle |
| `set_budget` | `/context size <N>` | `u32` (`Some(0)` = auto) | `size reset` |
| `set_compaction_trigger_policy` | `/context compaction <policy>` | `CompactionTriggerPolicyOverride::{Set, Reset}` | `compaction reset` |

The read surface is `context_manager(cfg, …)`, `context_features(cfg, manager,
feature_override, kind)`, `compaction_trigger_policy(cfg, …)`, plus the live
`memory.usage()` sections and `CompressCounters` already rendered by the REPL.

## 2. Source of truth — one owner per layer

Mirrors the `backend_panel.rs` table. Each layer keeps its existing owner; the
panel only edits through that owner.

| # | Layer | Canonical owner | The panel's part |
|---|-------|-----------------|------------------|
| a | session manager override | `chat.rs` REPL state (`manager_override: Option<ContextManager>`) | hands a `ContextManager` pick back; the REPL loop assigns it, exactly as it applies `ContextCommandResult.set_manager` today |
| b | session feature overrides | `chat.rs` `feature_override: ContextFeatures` | returns a `Vec<(ContextFeature, bool)>` diff; REPL applies via the same path as `set_feature` |
| c | session send-budget override | `chat.rs` budget slot fed by `set_budget` | returns `Option<u32>`; `Some(0)` = auto (probed value) |
| d | session compaction-policy override | `chat.rs` `compaction_policy_override` | returns `CompactionTriggerPolicyOverride` |
| e | config-file defaults (`[context]`, `[context.features]`) | `newt_core::Config` | **read-only here.** The panel shows config as provenance but never writes the file — there is no crash-safe plan machinery for it yet (#1660 is backend-drop-ins only). Editing config stays a text-file job. |
| f | live usage / compression counters | `memory.usage()` + `CompressCounters` (read-only) | rendered, never written |
| g | probe cache (`model-capabilities.json`) | `probe.rs` self-tuning, written by inference turns | **read-only display** (safe_context, max_ok_input, confidence). The panel shows it; it does not trigger probes. |
| h | in-panel dirty selection | `PanelState` dial — alive only while open | discarded on Esc; on Enter handed to (a)–(d) and never re-read |

Precedence when they disagree, most specific first: (h) at the moment of apply →
(a)–(d) session overrides → (e) config → newt's resolution defaults. That is
`context_manager` / `context_features` / `compaction_trigger_policy` resolution,
unchanged by this panel.

`PanelState` is pure (no terminal, no I/O) and unit-tested; the raw-mode loop
mirrors `config_panel::run` / `backend_panel::run`.

## 3. The headline flourish — a live budget gauge

This is what `/backend` *can't* offer and `/context` must: the send budget is a
**live, measured number**, not just a config string. The ROADMAP (24.6) already
wires a fill gauge into the RichTUI header via `set_runtime_context(used,
budget)`. The panel promotes that gauge to its header:

```
┌─ context ──────────────────────────────────────────────┐
│  ▓▓▓▓▓▓▓▓▓▓▓▓░░░░░░░░  6,412 / 24,000 tok  (27%)        │
│  budget: probed safe_context (high confidence)          │
│  manager: standard (session)   compaction: headroom_aware (config) │
└──────────────────────────────────────────────────────────┘
```

- The gauge reuses the exact `(used, budget)` pair the header already computes —
  no new measurement path.
- The "budget:" line names its **provenance** so the number is never magic:
  - `probed safe_context (high confidence)` — from `max_ok_input` /
    `safe_context` in the probe cache
  - `declared window` — from the model's `/api/show` context window
  - `session override` — operator set `/context size <N>`
  - `estimated` — nothing known yet

This is flourish #2 ("scale with a probe") made visible: the operator *sees*
that the budget came from empirical probing, and how confident the harness is in
it. A `p` row (see §5) jumps to the probe detail — but the probe itself is still
fired by `/backend probe` (or auto-tuning during inference), never by this panel
opening.

## 4. Grouping — by availability, not by type

Flourish #1. The flat `/context feature` list already annotates
`(pending #…)`; the panel makes availability the **structural** axis. Three
groups, in order:

```
  ▸ LIVE        standard · headroom_aware · tool_offload · scratchpad · semantic · experiential · scheduled
  ▸ OVERRIDDEN  (only rows whose session value differs from config)
  ▸ PENDING     progressive · distributed · provenance (#584)
```

- **LIVE** — `available() == true`. Toggleable. Rendered with their resolved
  on/off state.
- **OVERRIDDEN** — a *virtual* group that only appears when a session override
  is active. This answers the question the text command makes you hunt for:
  "what have I changed this session that isn't in my config?" Each row shows
  `config → session` (e.g. `off → on`). `x` on a row here resets that one
  override.
- **PENDING** — `available() == false`. Selectable to read the tracking-issue
  line (`#546`, `#584`), but **Enter is refused** with the existing message
  ("not yet available — staying on standard"). Same honesty rule as the text
  path: a config-forced-on pending feature shows its *actual* resolved state,
  never a hardcoded "off".

Why a virtual OVERRIDDEN group instead of a per-row dot: provenance dots are
compact, but they still make you scan six rows to find your one change. The
group collects them. (If it proves noisy in practice, fall back to the dot
column — both are cheap; the group is the stronger answer to "show me my
session drift.")

## 5. Layout — the lighter variant

Flourish #3. One screen, no tabs. The full per-section token breakdown stays in
`/context stats` (the existing pager); the panel shows only what you'd want to
*change* plus the one number you'd want to *watch*.

```
┌─ context ──────────────────────────────────────────────┐
│  ▓▓▓▓▓▓▓▓▓▓▓▓░░░░░░░░  6,412 / 24,000 tok  (27%)        │
│  budget: probed safe_context (high confidence)          │
│                                                          │
│  manager                                                 │
│    ● standard          (session)                         │
│    ○ progressive       not yet available — #546          │
│    ○ distributed       not yet available — #546          │
│                                                          │
│  features              ←/→ toggle · only LIVE apply      │
│    tool_offload    [on ]                                 │
│    scratchpad      [on ]   config off → session on       │
│    semantic        [off]                                 │
│    experiential  [on ]                                   │
│    scheduled       [off]                                 │
│    provenance      [off]   not yet available — #584      │
│                                                          │
│  compaction          ● headroom_aware  ○ message_count   │
│  send budget         [auto ▾]  probed 24,000 · /context size │
│                                                          │
│  compression: 2 this session · last reclaimed 41% · auto on │
│                                                          │
│  enter apply · esc cancel · x reset override · c compact now │
│  :stats full breakdown (pager)                           │
└──────────────────────────────────────────┘
```

"Light" cuts, deliberately:
- No per-section token table (that's `stats`).
- No feature *descriptions* inline — the keyword + pending-issue is enough; the
  text command's help already teaches the vocabulary.
- The compression block is **one status line** (`memory_compress_section`
  output, condensed), not the full counter dump.

## 6. Interaction grammar

Same grammar as `/backend` / `/psyche`, two context-specific verbs added:

| Key | Action |
|-----|--------|
| `←`/`→` | dial the focused row (manager preset, feature on/off, compaction policy, budget preset) |
| `Enter` | apply the dirty diff and close |
| `Esc` | cancel silently — Enter/Esc indistinguishable on a no-op visit (#1665) |
| `x` | reset the focused row's session override → back to config |
| `c` | compact now (fires the existing compression flow; see §8 Q1) |
| `:stats` | hand off to the `/context stats` pager on close |
| `e`/`a`/`d` | **absent** — no file editing (see §2 row e) |

Apply semantics: a single `ContextPanelOutcome { manager, feature_diff,
budget, compaction }` returned to the REPL, which applies each field through
the *same* code that applies `ContextCommandResult`. Nothing is applied until
Enter; a failed or refused row keeps the panel open with a visible status and
mutates nothing (config_panel review-3 §1).

## 7. Testing

- `PanelState` is pure and unit-tested like `backend_panel::PanelState`:
  - grouping puts `provenance`/`progressive`/`distributed` in PENDING, rest in LIVE
  - OVERRIDDEN group appears iff a session value ≠ config value
  - Enter on a PENDING row is refused, state unchanged
  - `x` on an OVERRIDDEN row produces the reset entry in the outcome
  - budget provenance label picks probed > declared > override > estimated
- The REPL apply path is covered by the existing
  `handle_context_command_dispatch` test pattern — the panel's outcome feeds the
  same appliers, so no new REPL logic needs a PTY test.

## 8. Open questions

1. **`c` compact-now** — does it reuse the REPL's compression entry point
   verbatim (preferred: one path), or is there a reason the panel needs a
   distinct trigger? Assumption: reuse.
2. **Budget dial** — is a preset dial (`auto / 8k / 16k / 24k / custom`) enough,
   or do we need free-text entry? Presets keep the panel light; free text is
   what `/context size <N>` is for. Assumption: presets + footer hint.
3. **Probing from the panel** — confirmed out of scope (panel displays the
   cache; `/backend probe` fires probes). If we later want a "re-probe this
   model" button, it belongs in the `/backend` panel that already owns
   probe UX, not here.

## 9. What this deliberately does NOT do

- **No config-file writes.** `backend_panel` can edit `~/.newt/backends/*.toml`
  because #1660 built crash-safe plan machinery for those drop-ins. No
  equivalent exists for `[context]` in `config.toml`. Until it does, the panel
  edits *session overrides only* and shows config as read-only provenance. This
  is the single biggest scope guard.
- **No new probe machinery.** The self-tuning cache already exists; the panel
  reads it.
- **No replacement of `/context stats`.** The pager keeps the detailed view.
