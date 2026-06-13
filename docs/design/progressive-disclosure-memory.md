# Design: progressive-disclosure memory (Workstream A)

**Status:** proposed — design note for the MVP that follows.
**Date:** 2026-06-13
**Tracking:** completes `#319` (summarization-induced hallucination); first of
the three "disclosure at three scales" workstreams
(`~/.claude/plans/harmonic-snuggling-flask.md`).
**Lineage:** extends `docs/design/context-memory-hermes-learnings.md`
(Phases 17-19) and `docs/notes/2026-06-13-summarization-induced-hallucination.md`
(the #319 finding this finishes).
**Scope of this note:** the design only. No implementation lands with this PR;
the MVP is a separate, additive PR (§9). Workstreams B (coder-symbolic memory)
and C (workflow swarm) are out of scope.

---

## TL;DR

Newt's memory is **frozen or lossily summarized**, never **navigated**:

- **NOTES** (`newt-core/src/notes.rs`) are read once at session start and
  `frozen` verbatim into the system prompt (`NoteStore::system_prompt_block`).
  Every byte rides in every request whether or not the turn needs it, and the
  char cap (`NoteStore::DEFAULT_CHAR_LIMIT = 2_200`) is the only governor.
- **History** is compressed by `newt-core/src/agentic/compress.rs`: the middle
  is replaced by a prose summary behind `SUMMARY_PREFIX`
  (`[CONTEXT COMPACTION — REFERENCE ONLY]`) and the **pre-compaction bytes are
  discarded** — only the summary survives. #319 showed this *induces*
  hallucination: a confident summary suppresses the model's instinct to
  re-read. The #321 fix appends a deterministic `reread_breadcrumb` naming the
  dropped files, but the breadcrumb is only a *pointer with no destination* —
  it says "re-read this" for files still on disk, and says nothing usable about
  a *past turn* whose bytes are gone.

The same **index-then-fetch** pattern is already proven three times in-tree
(skills, the recall tool, the modulex discovery trio). Memory just hasn't been
wired to disclose the same way.

**This design:** make memory a *budgeted, addressable resource the agent
navigates on demand*.

1. A budgeted **memory INDEX** in the working set — note titles/ids, past-turn
   keywords, and compaction markers — instead of freezing all NOTES and
   discarding the pre-compaction span.
2. A **`memory_fetch` tool** (mirrors `use_skill` / `recall`) that pulls a note
   body, a past turn, or the pre-compaction content behind a `SUMMARY_PREFIX`
   marker, on demand.
3. A **disclosure budget/facet** for memory (index vs. verbatim ratio),
   reusing modulex's `DEFAULT_TOOL_BUDGET` budget-pin idea.

The **load-bearing new piece** is retention: for a `SUMMARY_PREFIX` marker to
be *fetchable*, the pipeline must stop discarding the pre-compaction span and
write it somewhere addressable (the §6 store). That is §6 of this note.

This completes #319: the breadcrumb stops being a bare instruction and becomes a
real **"pull it back"** affordance — labelled absence with a recovery handle.

---

## 1. Thesis: memory is addressable, not frozen or summarized

The #319 note (`docs/notes/2026-06-13-summarization-induced-hallucination.md`)
established the epistemics: *a confident summary is worse than a labelled
absence, because absence routes the model to retrieval and a summary
suppresses that*. The fix there was honesty (label the gap). The next move is to
**give the labelled gap a destination** — an addressable handle the model can
actually pull.

State this as the through-line the plan calls "disclosure at three scales":

> Context is a budgeted, addressable resource the agent navigates on demand —
> not a blob you summarize and hope.

- **Within a turn** (#321, shipped): the `reread_breadcrumb` names files the
  model should pull back rather than recall.
- **Within a session** (this workstream): NOTES and history become an *index*
  the model queries, with `memory_fetch` as the pull.
- **Across agents** (Workstream C, deferred): an outer agent discloses curated
  sub-contexts to children.

Memory today fails the through-line in two distinct ways, and the design fixes
both:

| Surface | Today | Failure | This design |
|---|---|---|---|
| NOTES | frozen verbatim into the system prompt | every byte in every request; cap is the only governor | index titles/ids in prompt; bodies fetched on demand |
| history middle | summarized to prose, **bytes discarded** | #319: confident summary, original tokens unrecoverable | summary + marker stays; pre-compaction span retained and fetchable |

Both become: *a small index in the working set + a fetch tool for the verbatim
body*. The summary's prose is kept as the *cheap recall layer*; the verbatim
span behind it is the *expensive-but-exact layer* the model pulls only when it
needs the real signature, type, or line.

---

## 2. What already exists to reuse (three proven index-then-fetch patterns)

The mechanism is not novel in this codebase — it is the established way newt
keeps the prompt small while keeping the long tail reachable. Cite the seams so
the MVP copies them rather than reinventing:

### 2.1 Skills — `index_line` → `use_skill`

`newt-skills/src/lib.rs`:

- `Skill::index_line` renders one `name: description (when to use: …)` line.
- `index_block` assembles **only** those lines into the system prompt
  ("Available skills (call `use_skill` to load one):"). Never bodies.
- The `use_skill` tool (`newt-core/src/agentic/tools.rs` ~L99 schema, ~L1025
  executor) calls `newt_skills::load_body_from(dirs, name)` to pull the full
  `SKILL.md` body on demand.

This is the canonical "index in prompt, body on tool call" shape. `memory_fetch`
is `use_skill` for memory.

### 2.2 Recall — `RecallSource::search` over the §6 store

`newt-core/src/agentic/recall.rs`:

- `RecallSource` is a minimal trait the TUI injects (`StoreRecallSource`) so the
  loop never names `ConversationStore` directly — the same injection discipline
  as `NoteSink`. `recall_source: None` ⇒ the tool is not advertised (eval /
  headless unaffected).
- `recall_tool_definition` carries dense *coaching text* tuned for small local
  models: what it searches, when to reach for it, when **not** to (already in
  context), and "plain keywords, not boolean/FTS syntax".
- `execute_recall` returns *snippets* (id + title + `seq` + bm25-marked
  excerpt) — never full content. Every branch is a tool *result*, never a loop
  abort: a sanitizer-rejected query comes back as coaching, zero hits as "no
  matches…".

Recall already does index-then-*snippet* over **past conversations**.
`memory_fetch` is its sibling that pulls the **full body** of a specific
addressed item (a note, a turn, a compaction span) rather than ranked snippets.

### 2.3 Modulex discovery trio — budgeted faceted disclosure

`/home/hartsock/workspaces/modulex-mcp`,
`crates/modulex-mcp/src/tools.rs` + `facets.rs`:

- `tool_search` / `tool_describe` / `tool_invoke` (`h_tool_search` etc.) are the
  "constant-size long tail": the default surface lists a budgeted set and the
  rest is *discovered*, not crammed.
- `DEFAULT_TOOL_BUDGET = 12` (`tools.rs:742`) is **pinned by CI** — "growing it
  is a deliberate change to this constant with its own justification, never a
  side effect of a feature". `FacetPolicy` (`facets.rs`) resolves which facets
  are exposed; `exposes`/`denies` gate the listing.
- `tool_invoke` has a one-level re-entrancy guard ("cannot invoke the discovery
  tools") — a discipline `memory_fetch` copies (a fetch must not be able to
  fetch a fetch tool's schema, or trigger another fetch recursively).

This is the **budget-pin** the memory index borrows: an index size capped by a
single CI-pinned constant, not by whatever happens to accumulate.

---

## 3. Design

Three additive pieces, all plugging into the existing
`MemoryProvider` / `MemoryManager` seam (`newt-core/src/memory.rs`) **without
breaking** the shipped providers (`RollingWindow`, `TokenBudget`,
`Summarizing`, `NoteStore`, `SoulProvider`) or the #289/#307 surfaces
(`take_compaction_record`/`restore_turns` continuity, `/memory` counters).

### 3.1 A budgeted memory INDEX in the working set

A new provider — call it `MemoryIndex` — contributes a small, budgeted index
block via the existing `MemoryProvider::system_prompt_block` (frozen at session
start, KV-cache-safe) listing:

- **NOTE entries** by short id + first-line title (NOT the bodies). Sourced from
  the same `NoteStore` entries that exist today; the index references them,
  the bodies are fetched.
- **Compaction markers** present in the working set: one line per
  `SUMMARY_PREFIX` span, with its addressable id (§6) and a one-line topic
  hint. This is the destination the #321 breadcrumb has been missing.
- **Past-turn keywords** are *not* duplicated here — that is exactly what
  `recall` already provides (snippet search over the store). The index points
  at recall for that axis rather than competing with it.

The index block is governed by a single CI-pinned budget constant
(`MEMORY_INDEX_BUDGET`, the modulex `DEFAULT_TOOL_BUDGET` pattern — §2.3):
the index lists at most N items; beyond N, the oldest/least-relevant items are
*addressable via recall* but not listed. Growing N is a deliberate edit to the
constant with its own justification, asserted by a test (§10).

Crucially this is **additive**: `MemoryIndex` is a provider registered
alongside the existing ones, behind a config flag (§9). When the flag is off,
`MemoryManager` behaves exactly as today. The index provider's
`build_messages` returns `Vec::new()` (system-prompt-only, like `NoteStore` and
`SoulProvider`) so it never competes for the "first non-empty `build_messages`"
slot in `MemoryManager::build_messages`.

### 3.2 A `memory_fetch` tool

A new tool, registered the **same way** as `recall` and `save_note` (gated on an
injected source, not part of `tool_definitions`; advertised only when present —
`merged_tool_definitions` already threads `with_recall`/`with_save_note`
booleans, add `with_memory_fetch`). The seam is a minimal trait so the loop
never names `ConversationStore`/`NoteStore` directly:

```rust
/// Read-only pull of an addressed memory item. Workspace-fenced by the
/// underlying store, like RecallSource.
pub trait MemorySource: Send + Sync {
    fn fetch(&self, addr: &MemAddr) -> anyhow::Result<MemPayload>;
}
```

`MemAddr` is a small tagged address (`note:<id>`, `turn:<conv>#<seq>`,
`compaction:<id>`) — the same id forms the **index** (§3.1) renders, so the
model copy-pastes an address it was shown, exactly as `recall`'s short id pastes
into `/conversation restore`. `fetch` resolves:

- `note:<id>` → the full `NoteStore` entry body.
- `turn:<conv>#<seq>` → one past turn's verbatim user/assistant text
  (`ConversationStore` read; §6-ordered by `seq`, never re-sorted by clock).
- `compaction:<id>` → the **pre-compaction span** retained per §6 (the bytes
  behind a `SUMMARY_PREFIX` marker).

The executor (`execute_memory_fetch`, mirroring `execute_recall`) returns a
*tool result* in every branch — never a loop abort: unknown address → coaching
("addresses look like `note:…`, `turn:…`, `compaction:…`; copy one from the
memory index"), missing item → "no such memory item" (never empty), backend
failure → `error: …` verbatim. Coaching text is dense and small-model-tuned per
the recall lesson (§8).

### 3.3 A disclosure budget/facet for memory

Reuse modulex's budget-pin idea (§2.3) for the *index-vs-verbatim ratio*:

- `MEMORY_INDEX_BUDGET` caps how many items the frozen index lists (the cheap
  layer that always rides).
- A per-call cap on `memory_fetch` payload size bounds how much verbatim content
  one fetch can re-enter (the expensive layer), the same token-budget guard
  `recall` applies via `RECALL_MAX_LIMIT` — fetched bodies ride back through the
  model's context, so an unbounded fetch is a budget hole.

A `[memory] disclosure = "index" | "frozen"` config switch (default `frozen`
for the MVP, see §9) selects whether NOTES disclose progressively (index +
fetch) or stay frozen-into-prompt as today. This is the facet seam: it is about
*context cost*, never authorization (the leash governs effects, per modulex's
facet docstring).

### 3.4 How it plugs into `MemoryProvider`/`MemoryManager` (additive)

| Piece | Existing method reused | Additive change |
|---|---|---|
| Index block | `system_prompt_block` (frozen, KV-safe) | new `MemoryIndex` provider; `build_messages` ⇒ `Vec::new()` |
| Note bodies | `NoteStore` entries already in memory | `MemorySource::fetch` reads them; no new write path |
| Past turns | `ConversationStore` (already injected for recall) | reuse the recall injection; `fetch` adds a by-id read |
| Compaction span | `on_pre_compress` hook + `take_compaction_record` | retain the span at compaction time (§6); `fetch` reads it |
| Tool gating | `merged_tool_definitions(with_recall, with_save_note)` | add `with_memory_fetch`; `None` ⇒ not advertised |

No existing provider's trait impl changes. `MemoryManager`'s fan-out
(`build_system_prompt_additions`, `build_messages`, `on_pre_compress`,
`take_compaction_record`, `restore_turns`) is untouched in shape — `MemoryIndex`
is just another registered provider. The #289/#307 continuity surfaces
(`take_compaction_record` → store turn record → `restore_turns` rehydration)
keep working; §6 adds a *parallel* retention write, it does not alter the
compaction-record-as-turn flow.

---

## 4. The retention question (the load-bearing new piece)

Today the pre-compaction content is **discarded**. Trace it in
`newt-core/src/agentic/compress.rs::compress`:

1. `compute_boundary` splits `[head | middle | tail]`.
2. The `middle` is rendered, redacted, summarized, and **replaced** by a single
   `summary_message` carrying `SUMMARY_PREFIX`.
3. The original `middle` `Value`s are dropped on the floor — the assembled
   output is `head + summary + tail`. Only the prose summary survives.

The `Summarizing` provider then mints that summary message as a turn record
(`take_compaction_record`), so a *summary* is durable — but the **verbatim
middle bytes are gone**. There is nothing for a `compaction:<id>` fetch to
return. This is exactly why #321 could only emit a breadcrumb for files *on
disk* (re-readable) and had nothing to offer for a past *turn* whose content
was the summarized middle.

**For the breadcrumb to become a real pull, the pipeline must retain the
pre-compaction span somewhere addressable.** Three honest options:

| Option | Where | Cost | Verdict |
|---|---|---|---|
| A. §6 store (a `compaction_archive` row per compacted span) | `ConversationStore` | one extra write at compaction; workspace-fenced; survives restart | **proposed** |
| B. per-session in-memory archive (drop on session end) | `Summarizing` provider field | cheap; but lost on restart, and #319's whole point is *durable* recoverability | rejected (ephemeral defeats the purpose) |
| C. sidecar file per session | `~/.newt/…` | no schema; but reinvents the store's workspace-fencing, chaining, and pruning | rejected (the store already solves this) |

**Proposed: Option A — retain in the §6 store.** When `compress` produces a
`Summarized`/`StaticFallback` outcome, the pipeline (or the `Summarizing`
provider that calls it) writes the verbatim middle to a new
`compaction_archive` surface in `ConversationStore`, keyed by:

```
compaction:<conversation_id>#<compaction_seq>
```

where `compaction_seq` is the §6 per-writer tick at the moment of compaction
(monotonic, causal, never a wall-clock — §6 discipline; `ConversationStore`
already mints these via `next_tick`). The summary message's `SUMMARY_PREFIX`
text carries that same id, so the index (§3.1) lists it and `memory_fetch`
resolves it. The archive inherits the store's existing guarantees for free:
workspace fencing, atomic write-then-rename, the per-workspace prune cap, and
**secret redaction must run on the archived span too** (the compaction path
already runs `redact_secrets` on summarizer input — the archive write reuses the
same pass; a retained verbatim span is exactly as persistent as a summary and
must never carry credentials).

Retention is **bounded**: the archive is subject to the same
`max_per_workspace` prune discipline as conversations, and an over-budget
archive prunes oldest-span-first. An evicted span's `compaction:<id>` fetch
returns "no such memory item (pruned)" — labelled absence again, not a lie.

This is the single genuinely new persistence surface in the workstream;
everything else is wiring over existing seams. It is called out for adversarial
review (§8 risks; secret retention is the sharp edge).

---

## 5. Worked example (the #319 scenario, completed)

Replaying the #319 incident
(`docs/notes/2026-06-13-summarization-induced-hallucination.md` §3) with this
design:

1. Round 2: `read_file("src/api.rs")` → real signatures, verbatim.
2. Rounds 3-10: other work pushes `api.rs` out of the tail.
3. Round 11: compression fires. The middle (including the `api.rs` read) is
   summarized **and** archived to `compaction:<conv>#<seq>` (§6). The summary
   message carries the marker + the #321 `reread_breadcrumb` naming `src/api.rs`.
   The memory index (§3.1) now lists `compaction:<conv>#<seq>  (api.rs, callers)`.
4. Round 12: the model needs `connect`'s signature. Two honest recovery paths
   now exist, where before there was only hallucination:
   - the file is still on disk → re-read it (the #321 floor); **or**
   - the model `memory_fetch`es `compaction:<conv>#<seq>` and gets the verbatim
     pre-compaction tool result back — the exact bytes, no re-read round needed.

The breadcrumb's instruction ("re-read; do not recall from prose") now has a
*destination*. Confident loss → recoverable absence with a pull handle.

---

## 6. Does fetched content re-enter the working set? (and re-trigger compression?)

This is the sharpest interaction and is addressed explicitly rather than
hand-waved.

A `memory_fetch` result is a **tool result** — it enters the working set like
any other tool output, and is therefore subject to the *same* compression
pipeline that would fire on a large `read_file` result. That is acceptable and
desired: there is no special "fetched content is immune" path (such a path would
be the F1-class self-poisoning bug compress.rs guards against). The mitigations
are the existing ones plus one new guard:

- The per-call payload cap (§3.3) bounds a single fetch the way
  `RECALL_MAX_LIMIT` bounds recall — a fetch cannot re-enter an unbounded blob.
- A fetched `compaction:<id>` span, if re-compacted, must NOT be re-archived
  under a new id (that would let the archive grow without bound on repeated
  fetch→compact cycles). The retention write (§4) is **idempotent on the source
  span id**: re-compacting content that came from `compaction:<id>` reuses that
  id rather than minting a new one. This is the memory analogue of compress.rs's
  `is_compaction_message` / `is_compaction_text` guard — a fetched archive span
  is tagged so the pipeline recognizes it as already-archived.
- A fetch is a *deliberate* pull; if the model fetches a span and immediately
  blows the budget, the anti-thrash latch (`CompressState`, two sub-10% reclaims
  → disable) already protects against a fetch→compact→fetch oscillation. No new
  anti-thrash machinery is needed.

The honest statement: fetched content is normal context. The design does not
exempt it; it bounds it and prevents the archive from growing on cycles.

---

## 7. Interaction with the §6 store

- **Ordering.** Turn and compaction-span fetches are §6-ordered by `seq`, never
  re-sorted by `ts_claim` (the recall path's discipline,
  `RecallSource`/`ConversationStore::search` docstrings: "wall-clock is a
  display claim, not an ordering key"). `memory_fetch` returns content; it does
  not impose any ordering of its own.
- **Workspace fencing.** `MemorySource` is fenced to the active workspace by the
  store, exactly like `StoreRecallSource`. A `turn:`/`compaction:` address from
  another workspace resolves to "no such item", not a cross-workspace leak.
- **Chain integrity.** The `compaction_archive` rows are *content*, not part of
  the turn chain's `(writer_fingerprint, seq)` ordering of conversation turns —
  they are a parallel, addressable side-table keyed off the conversation id.
  Whether they participate in `verify_chain` is an open question (§8): the
  conservative default is that they are auxiliary content like `events`, hashed
  if cheap, not a new chain.
- **Pruning.** Archive spans prune with their conversation (`delete` already
  cascades; the per-workspace cap applies). No orphan spans outlive their
  conversation.

---

## 8. Open questions / risks

1. **Secret retention (sharp edge).** A retained verbatim span is as persistent
   as a summary and must run the *same* `redact_secrets` pass before it hits the
   archive. Risk: the summarizer path redacts its *rendered* input, but the
   archive wants the *original* `Value`s. The MVP must redact the archived span
   too (the same table), and a test must prove a credential in the middle never
   reaches the archive. **Gets adversarial review.**
2. **Budget tuning.** What is `MEMORY_INDEX_BUDGET`? Start small (≈ the
   `DEFAULT_TOOL_BUDGET = 12` order of magnitude) and pin it; tune empirically
   against the probe-and-ratchet capability data, never down silently.
3. **Re-entry / re-compression** (§6) — bounded by the per-call cap and the
   idempotent-on-source-id retention rule; needs an explicit test that a
   fetch→compact→fetch cycle does not grow the archive.
4. **Small-model coaching.** The recall lesson is that dense, example-laden
   schema text ("'like we did before'", "plain keywords, not boolean syntax")
   is load-bearing for weak local models. `memory_fetch`'s description must show
   the address forms by example and say when to reach for a fetch vs. a re-read
   vs. a recall. Under-coached, a small model either never fetches or fetches
   garbage addresses.
5. **Index vs. recall overlap.** The index must *point at* recall for the
   past-turn-keyword axis, not duplicate it, or the prompt grows for no benefit.
6. **`verify_chain` participation** of archive rows (§7) — defer to the MVP's
   store work; conservative default is auxiliary content.
7. **Frozen-prompt tension.** The index is frozen at session start (KV-cache).
   Notes/compactions created *mid-session* won't appear in the index until next
   session — the same accepted limitation `NoteStore`'s frozen snapshot has
   today. `memory_fetch` by a known id still works mid-session even when the
   index hasn't refreshed; the index is a convenience surface, the fetch is the
   capability.

---

## 9. MVP scope vs. full

### MVP (the smallest first PR, additive, flag-gated)

1. The `memory_fetch` tool: `MemorySource` trait + `StoreMemorySource` impl +
   `memory_fetch_tool_definition` + `execute_memory_fetch`, gated on an injected
   source via `merged_tool_definitions(with_memory_fetch)` — exact mirror of the
   recall wiring. Resolves `note:` and `turn:` addresses (both read existing
   surfaces; **no new persistence**).
2. A budgeted `MemoryIndex` provider (note titles/ids + present compaction
   markers) behind `[memory] disclosure = "index"`, default off (`frozen`) so
   the MVP changes nothing unless opted in.
3. The `MEMORY_INDEX_BUDGET` constant, CI-pinned (§2.3 pattern).

The MVP deliberately ships `note:` + `turn:` fetch **without** the new retention
surface, because both read content that already exists. It still completes the
*tool + index + budget* mechanism and proves the disclosure shape end-to-end.

### Deferred to the full workstream (next PR)

- **§4 retention** of the pre-compaction span (`compaction_archive` in the §6
  store) + `compaction:` address resolution. This is the load-bearing piece for
  the #319 completion and carries the secret-retention review; it is sequenced
  *after* the MVP proves the fetch+index mechanism, so the risky persistence
  change lands on a working foundation.
- Empirical budget tuning against probe data.
- Index/recall convergence polish.

### Explicitly out of scope

- Any production behavior change when the flag is off (the MVP is inert by
  default).
- Workstream B (coder-symbolic memory) and Workstream C (workflow swarm).
- A vector store / embeddings recall (recall is FTS5 bm25 by design — the
  hermes study's "snippet is enough" conclusion stands).

---

## 10. Test plan + acceptance

Acceptance is the standard contract (`docs/ROADMAP.md`): `cargo build`,
`cargo test`, `cargo clippy -D warnings`, `cargo fmt --check`, and the **80%
coverage floor** (`just cov-ci`) — ratchets up, never down.

Workstream-specific tests, all deterministic, no live model, tempdir store
(`tempfile`), mock source for the tool (mirroring `recall.rs`'s `MockSource`):

| Behavior | Test |
|---|---|
| Tool gating | `memory_fetch` advertised iff a `MemorySource` is injected (mirrors `recall` gating) |
| Note fetch | model fetches `note:<id>` against a tempdir `NoteStore`, gets the verbatim body |
| Turn fetch | model fetches `turn:<conv>#<seq>` against a tempdir `ConversationStore`, gets the verbatim turn; §6 `seq` order preserved |
| Compaction fetch (full workstream) | model fetches `compaction:<id>`; the retained pre-compaction span comes back verbatim — the #319 probe's `api_signature_survived` flips to **true** via fetch |
| Index under cap | the index block lists ≤ `MEMORY_INDEX_BUDGET` items; a CI-pinned assertion fails if the default surface exceeds it (the modulex `DEFAULT_TOOL_BUDGET` test pattern) |
| Coaching | schema text shows the three address forms by example and distinguishes fetch vs. re-read vs. recall (the `recall.rs` schema-text test pattern) |
| Tool-result branches | unknown address → coaching, missing item → "no such item", backend error → `error:` verbatim — never an empty string, never a loop abort |
| Secret retention (full) | a credential in the compacted middle is `[REDACTED]` in the archive — never reaches disk |
| No re-entry growth (full) | a fetch→compact→fetch cycle reuses the source `compaction:<id>` and does not grow the archive |
| Flag off is inert | with `disclosure = "frozen"` (default), `MemoryManager`'s output byte-matches today |

---

## 11. Phasing

| Phase | Deliverable | Persistence change | Review |
|---|---|---|---|
| Design (this note) | `docs/design/progressive-disclosure-memory.md` | none | docs-only |
| MVP PR | `memory_fetch` (`note:`/`turn:`) + `MemoryIndex` + budget constant, flag-gated off | none (reads existing surfaces) | standard CI gate |
| Full PR | §4 `compaction_archive` retention + `compaction:` resolution; budget tuning | **new §6 store surface** | adversarial (secret retention) |
| Follow-on | index/recall convergence; empirical budget tuning | none | standard |

A and B share the "index" abstraction (B specializes it for code symbols); C
generalizes disclosure to the orchestration layer. This note is the seed the
other two reuse.

---

## References

- #319 finding: `docs/notes/2026-06-13-summarization-induced-hallucination.md`.
- #321 in-turn seed: `newt-core/src/agentic/compress.rs::reread_breadcrumb`,
  `SUMMARY_PREFIX`, `is_compaction_text`.
- Reuse seams: `newt-skills/src/lib.rs` (`Skill::index_line`, `index_block`,
  `use_skill`); `newt-core/src/agentic/recall.rs`
  (`RecallSource`, `recall_tool_definition`, `execute_recall`);
  `newt-core/src/agentic/tools.rs` (`merged_tool_definitions`, `use_skill`
  executor); `newt-core/src/store.rs` (`ConversationStore::search`,
  `append_turn_full`, `next_tick`, §6 ordering); `newt-core/src/notes.rs`
  (`NoteStore`, frozen `system_prompt_block`).
- Budget-pin pattern: `modulex-mcp/crates/modulex-mcp/src/tools.rs`
  (`tool_search`/`tool_describe`/`tool_invoke`, `DEFAULT_TOOL_BUDGET = 12`) and
  `facets.rs` (`FacetPolicy`, `DEFAULT_FACETS`).
- Provider seam: `newt-core/src/memory.rs` (`MemoryProvider`, `MemoryManager`,
  `system_prompt_block`, `build_messages`, `on_pre_compress`,
  `take_compaction_record`, `restore_turns`).
- Plan: `~/.claude/plans/harmonic-snuggling-flask.md` (Workstream A).
- Lineage: `docs/design/context-memory-hermes-learnings.md` (Phases 17-19).
