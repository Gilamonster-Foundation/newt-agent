# RAG as default-on context retrieval

**Status:** proposal (revised after an adversarial critique pass — see
"Critique log" at the end). Companion: [`../ollama-assist-learnings.md`](../ollama-assist-learnings.md).
Related: `context-management-modes.md` (#582 shortlist), `progressive-disclosure-memory.md`
(the conversation-recall rejection), `improving-crew-results.md` (the A/B instrument).

## The ask, restated precisely

> "Implement RAG as an integral part of the context management system … a
> default-on feature that works on a code base." Plus, on provisioning: "the
> candle backend [should] auto-download the [embedding] model … make sure the
> provenance is sound … we may be running with a vLLM backend or on a system
> without local Ollama."

Two things to get right before anything else.

**First: newt-agent already has embedding-based RAG-for-code.** It is
`newt-core/src/agentic/semantic.rs` (issue #582, "Step 26.5"), it is real,
unit-tested, and wired into the live turn loop. It is *not* default-on and
*not yet* robust enough to be. So this is a **harden-the-existing-engine** plan,
not greenfield — framing it as build-from-scratch would duplicate a working,
tested engine and is the biggest mistake available here.

**Second — the honest caveat the critique forced to the front:** *the engine
being tested proves it chunks/embeds/renders correctly; it does not prove the
retrieved `<code_evidence>` block improves task success.* There is not one
ablation attributing a gain to retrieval. The operator has made the product
call to pursue default-on; this plan's job is to make that **safe, provisioned
out-of-the-box, and measured** — and to say plainly what measurement would
retract the default.

**One feature, two faces (push *and* pull).** `/context feature semantic on`
*is* the RAG feature, and it enables both gestures at once: the head-of-turn
`<code_evidence>` **injection** (push) *and* the `code_search` **tool** (pull)
— the tool's searcher exists only when the semantic embedder does
(`lib.rs:6544` `code_search: semantic_embedder.as_deref().map(...)`, itself
`semantic_on.then(...)` at `:6257`). So push-vs-pull is **within-feature
tuning, not an either/or between features** — turning `semantic` on turns on
both, and the design question is how to *balance* them, not which to pick.

**Observed tool-shape problem (operator report, worth acting on):** with the
feature on, the model is seen calling `code_search` **repeatedly** within a
turn — burning tool rounds and suggesting the pull half's *shape* is wrong
(returns too little per call, results don't stay salient in context, or the
model re-searches because it can't tell it already has the answer). This
matters two ways: (a) it weakens the tidy "just prefer the on-demand tool"
recommendation — the pull gesture has its *own* failure mode, not a clean win
over the push; and (b) it interacts with the in-flight `workflow_grace_rounds`
driver change (progress-aware rounds past `max_tool_rounds`) — repeated
searches consume exactly the round budget that work adjusts. So the tool shape
is a first-class design surface here, not a settled given (see Phase 4 and the
Sequencing note).

## What exists today (verified on `origin/main` @ 8297e96)

All in `newt-core/src/agentic/semantic.rs` unless noted; line numbers spot-checked.

- **`Embedder` trait** (`:21`) — mockable seam, `async fn embed(&self, text) -> Result<Vec<f32>>`.
- **Two embedders**: `EmbeddingsClient` (`:33`) speaks Ollama `/api/embeddings`
  and OpenAI `/v1/embeddings` (vLLM) with retry/backoff/auth; `CandleEmbedder`
  (`newt-inference/src/embed.rs`, #720) runs `BAAI/bge-small-en-v1.5` (384-dim)
  **in-process on CPU, off the chat model's VRAM**.
- **Structure-aligned chunking** (`chunk_source`, `:166`) — cuts at
  `symbols::extract_definitions` boundaries (with the leading doc-comment),
  `CHUNK_MAX_CHARS = 2000`, oversized bodies → 40-line windows, unknown
  languages → line windows. Already better than OllamAssist's blind 3000-char
  `substring`.
- **Index** (`SessionSemanticIndex`, `:276`) — `Mutex<Vec<(CodeChunk, Vec<f32>)>>`,
  brute-force cosine, top-k. In-memory, session-scoped, no persistence, no ANN.
- **Retrieval** (`retrieve_evidence`, `:468`) — embed the query, over-fetch
  `top_k*3`, rerank with `DEF_BOOST`/`PATH_BOOST` (0.05 each), render a
  `<code_evidence>` block capped at 6000 chars. Returns `None` (a safe no-op)
  when nothing embeds or matches.
- **Discovery** (`gather_code_files`, `:487`) — gitignore-aware
  (`ignore::WalkBuilder`), **`.rs`/`.py` only**, **400-file / 200 KB caps**
  (`MAX_FILES`/`MAX_BYTES`, `:488-489`).
- **A `code_search` tool** (`:521`) exposed to the model when the feature is on.

Live wiring, `newt-tui/src/lib.rs`:

- First active turn (guarded by `semantic_indexed`, reset on `/new`): the
  workspace is walked and indexed once, and `retrieve_evidence` injects an
  ephemeral `<code_evidence>` block at the head of the turn system prompt every
  turn (never persisted).
- **Both the index build (`:6281`) and the per-turn query embed (`:6307`) run
  synchronously via `tokio::task::block_in_place(rt.block_on(...))`** — i.e. on
  the turn's critical path. `index_files` embeds **one chunk per round-trip,
  sequentially**. This is the central latency risk (see Phase 0.5).
- **Already degrades gracefully**: 0 chunks → prints "semantic: indexed 0
  chunks — is the embedding model '…' pulled in Ollama? (retrieval is a no-op
  until it is)" and continues. No turn ever fails on a missing model.

Default posture, `config.rs:964` (`ContextFeatureSet::base_for`): for
`BackendKind::Ollama` it sets `scratchpad = true; scheduled = true` — `semantic`
is untouched, so it inherits off. Flipping it is two lines; the reason it isn't
already flipped is everything below.

## The provisioning decision (resolves vLLM / no-Ollama / dgx VRAM)

The chat backend **cannot** be relied on for embeddings:

- **vLLM** has no embeddings-model-pull mechanism, and may not expose an
  embeddings endpoint at all.
- **No local Ollama** at all is a supported deployment.
- **Ollama on a shared DGX** *can* serve a second model, but under VRAM pressure
  loading the embedder **evicts the chat model** (Ollama unloads LRU to fit),
  costing a cold reload on the next chat turn — the exact thrash #720's module
  doc names as its reason to exist. There is also **no `POST /api/pull` HTTP
  client in the codebase** to auto-pull with; `dgx.rs` shells `ollama pull` over
  *SSH* and only speaks `GET /api/tags` over HTTP. So "auto-pull via Ollama" is
  neither robust (VRAM/eviction) nor already-built (no client).

**Therefore the default embedding path is the in-process `CandleEmbedder`**
(CPU, off-VRAM, backend-agnostic). It works identically whether chat is Ollama,
vLLM, or nothing. The remaining question is how its model gets onto disk, since
today it refuses to auto-download (the #639 "no silent download into a small
box" law).

### Provisioning, with sound provenance (a deliberate, principled relaxation of #639)

Two complementary mechanisms; `sha2` + `reqwest` are already workspace deps, so
both are buildable today.

1. **Provenance-verified auto-download (the lean-binary default).** On first
   use, if the embedded model dir is absent, fetch it once from a **pinned HF
   revision (a commit SHA, never a moving tag)** and verify the **SHA-256 of
   every file** (`config.json`, `tokenizer.json`, `model.safetensors`) against
   hashes **compiled into the binary as constants**. Refuse to load on any
   mismatch. The binary carries the *proof*; the bytes must match it — this is
   the ContentAddressable thesis applied to a model. This **relaxes #639
   deliberately**: it is not *silent* (explicit one-time "fetching bge-small
   (~130 MB) for local code retrieval…" notice) and not *unverified*
   (content-addressed). Config escape hatch `[context.semantic] auto_fetch`
   (default on) lets an air-gapped user forbid it.
2. **Build-time bundling (the batteries-included artifact).** A cargo feature
   `bundled-embedding-model` `include_bytes!`s the ~130 MB `bge-small` weights
   into the binary: off for lean / wyvern / headless builds, on for a release
   artifact that must work fully offline. Provenance is trivially sound
   (verified at build time). Best of both: bundled binary for air-gapped
   installs, provenance-verified fetch for the lean one.

**On "auto-download `nomic-embed-text` specifically":** the candle loader can't
run it today — candle-transformers 0.8 has standard BERT but no `nomic_bert`
(nomic uses rotary + SwiGLU), which is why #720 chose standard-BERT `bge-small`.
**Decision: the auto-provisioned in-process default is `bge-small-en-v1.5` for
now** (zero loader work, already supported); `nomic-embed-text` stays available
over the HTTP/Ollama path for anyone who configures it. A stronger in-process
model — a `nomic-bert` loader, a code-specialized embedder, or a no-loader-work
swap like `gte-small` — is **filed as research issue #943** for a future pass,
decided by the same three-arm ablation harness. Not blocking; bge-small ships
the feature.

## Sequencing / prerequisites (this is plan-only until these land)

**Implementation is gated on in-flight work landing first — do not start
building against the current tree.** Specifically, the `workflow_grace_rounds`
change (**PR #942**: configurable `[tui].workflow_grace_rounds`, default 5,
per-model override, `0` = hard cap; threaded through `driver.rs` → `ChatCtx` →
both chat paths) lands *before* any RAG implementation begins. It grants
progress-aware rounds past `max_tool_rounds` when an active plan step shows
**recent concrete progress** — where progress is a successful
`write_file`/`edit_file` or an `update_plan` (`meaningful_workflow_progress`),
within a 3-round horizon. **Read-only searches do *not* count as progress** —
so a model thrashing on `code_search` earns no grace and exhausts the normal
budget faster than one that edits. That directly informs Phase 4b: the
tool-shape work must be designed against the post-#942 driver, and a cleaner
search shape is worth *more* under grace (fewer/complete searches → sooner to
an edit → reaches the progress-grace path). Treat everything below as a design
to implement *after* #942 merges.

## Plan

Ordered so the risky default-flip comes *after* the three things that de-risk
it: evidence (Phase 0), a non-blocking turn (Phase 0.5), and out-of-box
provisioning (Phase 1).

### Phase 0 — Measure it, as a three-arm ablation (the real evidence gate)

The critique's central methodological catch: an off-vs-on A/B is the *wrong*
experiment, because a `code_search` tool already ships and the dominant code
query ("where is `parse_config` defined") is an exact-identifier lookup that
**BM25/git-grep serves better than embeddings** — a fact this plan's own Phase 4
concedes. A clean off-vs-on LIFT would prove only "some retrieval helps," which
the free git-grep/FTS path might already deliver at zero provisioning cost.

So Phase 0 is a **three-arm** run, n≥5/arm, on the `newt-eval/cases/*` set
(all 11 have hidden `grade_spec.rs` graders), reusing `sweep.sh` + `/ab-gate`:

- **A: off** (no injected evidence).
- **B: FTS/git-grep-only** — reuse `scope_grounding.rs` (#840) grep-derived
  candidates and/or a `ConversationStore`-style FTS5 index over code chunks. No
  embedding model, no first-turn embed pass.
- **C: embeddings** (today's `semantic.rs`).

Decision rule (a *real* gate, not advisory):

- **C beats both A and B** by a margin that pays for model-provisioning +
  first-turn latency + per-turn budget ⇒ default-on embeddings is justified.
- **C ≈ B, both beat A** ⇒ default the **cheap FTS path**, keep embeddings
  opt-in. (This is the honest, likely-cheaper win.)
- **B ≈ C ≈ A (NO-LIFT within power)** ⇒ keep retrieval **opt-in**; ship the
  `code_search` tool as the default gesture instead of head-of-turn injection.
- **Any arm shows a CI-significant REGRESSION** ⇒ hard block that arm as a
  default; injecting mediocre evidence can crowd out and mislead a small local
  model.

Also produce a **cold-start latency measurement** (fresh box, no model, this-
workspace-sized repo) covering the model fetch *and* cold-cache first-run index
— the numbers a first-run user actually feels — not just warm eval-case timing.

The operator may override the default toward on regardless; the point of Phase 0
is that the override is then made **knowingly**, and that we probably discover
the cheaper FTS default is as good — a better product outcome.

### Phase 0.5 — De-block the turn (a hard precondition for any default)

Independent of the evidence result, default-on is unacceptable while indexing
blocks the first turn. Today both the index build (`lib.rs:6281`) and the
per-turn query embed (`:6307`) are `block_in_place` on the critical path, and
`index_files` embeds serially. Before any flip:

- Move indexing **off** the critical path — spawn it as a background task;
  `retrieve_evidence` returns its existing safe `None` until the index is ready;
  surface a non-blocking "indexing N/M…" line.
- **Concurrency-bound** the embed (`buffer_unordered`) instead of the serial
  per-chunk loop.
- **Time-box / make cancelable** the per-turn query embed so a slow embeddings
  host can't wedge each turn.

### Phase 1 — Out-of-box provisioning via the candle backend

Implement the candle default + provenance-verified fetch (and the optional
bundling feature) from the section above. Net: a fresh install on Ollama, vLLM,
or a bare box gets working local embeddings with **zero user setup and no VRAM
contention with the chat model**. The fetch is background + cancelable (folds
into Phase 0.5), never a blocking first-turn stall.

- **Phase 1b (follow-up):** a `nomic-bert` candle loader if in-process
  `nomic-embed-text` is wanted over `bge-small`.

### Phase 2 — Flip the default (local backends only), ship *with* Phases 0.5+1

```rust
if matches!(kind, BackendKind::Ollama) {
    base.scratchpad = true;
    base.scheduled = true;
    base.semantic = true;   // only if Phase 0 arm C (or B) earned it
}
```

Same posture as `scratchpad`/`scheduled`: local gets it; cloud/OpenAI-wire
backends stay opt-in (no guaranteed embeddings endpoint; "local vLLM reports as
Openai", `config.rs:961`). Explicit `[context.features] semantic = false` still
wins. **Never flip before Phase 0.5 (non-blocking) and Phase 1 (provisioned)
land** — default-on that stalls or silently no-ops is worse than opt-in. If
Phase 0 favors arm B, "the default" here is the FTS path, not embeddings.

**Confirm-on-disable UX.** Because the operator's thesis is that retrieval
**grounds the model against real code and reduces fabrication** (i.e. turning
it off degrades the model's ability to *verify* its own claims — consistent
with this repo's fabrication findings and the `symbols.rs` verify oracle),
`/context feature semantic off` should present a one-line confirmation rather
than silently disabling: *"semantic retrieval grounds answers in your actual
code; disabling may increase fabrication — turn off anyway?"* Two honesty
constraints on this:
1. **Justify it with the Phase 0 number, not an assertion.** The confirm text
   should cite the measured off-vs-on delta once we have it. If the ablation
   shows off does *not* degrade results, drop the confirm — don't warn about a
   harm we didn't observe.
2. **The confirm must not block the kill path** (see Rollback). A light
   confirm on "disable the feature" is fine; a hung/slow first-turn index must
   still be **immediately cancelable** without clearing the prompt. Confirm the
   *policy* change, never gate the *escape hatch*.

### The wider feature default posture (operator asked "shouldn't most be on?")

Not a blanket flip — the features split into two kinds, and the split is the
answer:

- **Protective / structured-state** (`scratchpad`✓, `scheduled`✓, and
  **`tool_offload`** — worth adding): they cap budget or maintain a ledger the
  model reads back. They don't inject guessed content, so none of the
  "adds-noise" risk applies. `tool_offload` (#584: cap oversized tool output,
  spill the full payload to a re-readable store) is a defensible default-on now.
- **Additive / retrieval** (`semantic`, `experiential`): they inject content
  the model didn't request, so each must clear the same three-arm evidence bar
  before defaulting on. `semantic` is this plan. **`experiential`** (#585,
  write-gated cross-task experience memory) is *also* a no-op on a fresh install
  (empty store) — low early value, same unmeasured risk — so keep it opt-in
  until measured.
- **`provenance`** (#584) is not implemented (`available() == false`) — it
  cannot be defaulted on; it's a no-op stub until it lands.

So the honest "most on" is: `scratchpad`, `scheduled`, `tool_offload`, and
`semantic` (this plan) default-on for local backends; `experiential` opt-in
pending its own measurement; `provenance` blocked on implementation.

### Phase 3 — Robust across a whole codebase

1. **Persistence, keyed correctly** (biggest steady-state win; also fixes the
   cold-first-run tax). Cache **`text_hash → vector`**, *not* `content_hash →
   CodeChunk` — the critique's catch: `CodeChunk` carries `file`/`start_line`/
   `end_line`, so keying the whole chunk by content hash mislabels identical or
   moved content with a stale `file:line` the agent would then fail to read.
   Rebuild `{file,start_line,end_line}` from the **current** walk every session;
   only the expensive vector is cached. **Namespace the cache by
   `(embedding_model, embedding_dim, chunker_version)`** and wipe-on-mismatch —
   without this, switching models silently yields all-zero cosines (dim
   mismatch → `cosine()==0.0`, `semantic.rs:313`) with no error; this is the
   `INDEX_VERSION` lesson from OllamAssist, which the first draft dropped.
   Store **per-workspace** (a subdir keyed by workspace root), `0600`.
2. **Secret hygiene (a security requirement this phase introduces, not
   optional).** `CodeChunk.text` is verbatim source, so an on-disk cache
   persists plaintext of any secret-bearing file. Before embed *or* persist:
   (a) a built-in denylist honored regardless of LanguagePack (`.env*`,
   `*.pem`, `id_*`, `*.key`, `*credential*`); (b) a cheap high-entropy/key-shape
   scan that skips a matching chunk; (c) cache `0600` under the per-workspace
   dir; (d) an explicit threat model in the doc. State it plainly: **turning
   this on writes readable source (minus denylisted/secret-shaped chunks) to a
   local cache.**
3. **Incremental re-index on file change** — once persistence exists, re-hash +
   re-embed a file when a write lands (the agent's own edits are the common
   case; name the write seam), **debounced and off the critical path** (the
   `Debouncer` lesson from OllamAssist), so retrieval sees edits without `/new`.
4. **De-hardcode coverage via `LanguagePack`** — `gather_code_files`'s
   `.rs`/`.py` list and the chunker's language set come from the same pluggable
   `LanguagePack` data (`api_surface.rs`) that already drives symbol extraction
   (three-Cs). **Raise/remove the 400-file cap under persistence, and print a
   visible "indexed 400/1,200 files (cap) — retrieval is partial" notice when
   truncated** — silent partial indexing ships confident-looking partial
   context, worse than none. This cap, not ANN, is the real first scaling wall.
5. **Scale the index structure** — brute-force cosine is fine at mid-repo scale;
   a large monorepo (once the cap is lifted) needs an approximate index
   (`hnsw`/`usearch`, or SQLite + `sqlite-vec` which also gives persistence).
   Gated on the cap being lifted (4) *and* real size, not on Phase 0.

### Phase 4 — Retrieval quality: hybrid retrieval *and* tool shape

Two threads, both informed by Phase 0.

**(a) Hybrid KNN + BM25 (adopt OllamAssist's best idea).** Vector KNN *and*
BM25, fused by Reciprocal Rank Fusion (`k=60`) — exact-identifier code queries
are common and BM25 nails them where embeddings miss. `semantic.rs` already
leans this way (`DEF_BOOST`/`PATH_BOOST`); a real BM25 arm (reusing the
`ConversationStore` FTS5/bm25 pattern + `sanitize_fts5_query`) is the principled
version. **If Phase 0's arm B is competitive, this hybrid is arguably the
product** — embeddings and BM25 together, not embeddings alone.

**(b) Fix the `code_search` tool shape (the pull half).** The operator observes
the model calling `code_search` repeatedly — a shape smell. Candidate fixes to
evaluate (the tool half is under the same `semantic` feature as the injection,
so this ships together): (i) **return more, structured, per call** — enough that
one search answers, with file:line anchors the model can cite instead of
re-searching; (ii) **make results sticky** — a retrieved chunk stays referenced
in context for the turn rather than evaporating, so the model doesn't re-fetch
what it already has; (iii) **tune push-vs-pull balance** — e.g. inject on the
first turn (cold, no query yet) but lean on the tool once the model has a
concrete query, instead of pushing a guessed block *every* turn; (iv) **round
budget** — note that under #942, read-only searches don't earn grace rounds
(only writes/edits/plan-updates do), so a search-thrashing loop already
exhausts budget faster than an editing one; the shape fix should convert
"search again" pressure into "edit now" progress sooner, rather than trying to
buy more search rounds. Measure shape changes on the same harness —
repeated-search count and tool-round spend are cheap, direct metrics alongside
task success.

## Composition with the rest of the system

- **vs. `scope_grounding.rs` (#840, git-grep authority fence).** Different
  questions: scope-grounding = "which files may this delegated sub-agent
  *write*"; semantic = "what code is *relevant* to this query." Complementary.
  Two seams: (a) a `code_search` hit feeds `ground_scope`'s `declared`
  augmentation when grep under-recalls (renamed symbols, cross-language
  callers); (b) — the OCAP one — a delegated crew/team leaf's `code_search`
  results should be **fenced by its subtask scope**, so retrieval can't hand a
  leaf evidence outside its authorized lane. Doesn't exist yet; natural next
  authority seam once both merge.
- **vs. conversation `recall` (FTS5) + the `progressive-disclosure-memory`
  rejection.** That doc rejected a vector store *for conversation recall*
  (FTS5/bm25 is enough — the hermes "snippet is enough" finding). **Scoped to
  conversation history; does not transfer to code** — but note the critique's
  fair point that the *transfer is unproven*, which is exactly what Phase 0 arm
  B tests. We do not assert code embeddings win; we measure it.
- **OCAP / caveats.** `semantic.rs` + `code_search` are read-only, scoped to the
  workspace root (`gather_code_files` never escapes it) — no new authority from
  turning retrieval on. The persistence cache is the one new artifact; secret
  hygiene (Phase 3.2) is its price of admission.

## Testing discipline

- Pure pipeline (chunk/embed/index/rerank/render) stays fully mocked
  (`MockEmbedder`, `wiremock`).
- Provenance fetch: `wiremock` the HF resolve URLs; unit-test "hash matches →
  load", "hash mismatch → refuse + clear error", "absent + auto_fetch=false →
  graceful no-op + nudge". No real network in the unit tier.
- Persistence: inject the fs seam (cache is pure over an injected store);
  in-memory fake, no `tempfile` in the unit tier. Test the version-namespace
  wipe-on-mismatch and the `text_hash → vector` (not whole-chunk) keying
  explicitly.
- Secret hygiene: unit-test the denylist + entropy-skip against fixture
  strings.
- The default flip: config test asserting `base_for(Ollama).semantic` matches
  the Phase-0-decided default and `base_for(Openai).semantic == false`.
- End-to-end proof is Phase 0's three-arm ablation, not a unit test.

## Rollback / kill story (the critique's gap)

- **Runtime off-switch:** `/context feature semantic off` mid-session (with the
  light confirm-on-disable above), and — distinct from disabling the feature —
  a slow/hung first-turn background index is **immediately cancelable** (folds
  into Phase 0.5) with *no* confirm. The distinction is the point: confirm the
  policy change ("keep the feature off from now on"), never gate the escape
  hatch ("stop this index that's wedging me right now"). A user who hits a slow
  index is never stuck behind a prompt.
- **Cleanup:** `newt semantic gc` (or documented cache path) purges the on-disk
  index — this is also the secret-remediation lever for Phase 3.2. Disabling the
  feature ignores/removes the on-disk index.
- **Kill criterion in success:** "the feature disables at runtime and cancels
  in-flight indexing without a restart, and disabling drops the on-disk index."

## Explicitly rejected / deferred

- **Rebuild a new RAG engine** — rejected; `semantic.rs` exists, tested, wired.
- **Auto-pull the embedding model via Ollama `/api/pull`** — rejected; no such
  client exists, and it's the VRAM-eviction path on a shared DGX. Candle
  self-provision replaces it.
- **Silent/unverified auto-download** — rejected; provisioning is explicit +
  SHA-256-verified against compiled-in hashes, or build-time bundled.
- **Default-on for cloud/OpenAI-wire backends** — deferred; no guaranteed
  embeddings endpoint.
- **ANN before raising the discovery cap** — deferred; the 400-file cap is the
  real wall and comes first.
- **A vector store for conversation recall** — still rejected (that decision
  stands); this plan is code retrieval only.
- **Flipping default-on before Phase 0 (evidence), 0.5 (non-blocking), and 1
  (provisioned) land** — rejected as the no-op / stall trap.

## Success criteria

1. Fresh install on **Ollama, vLLM, or a bare box** retrieves relevant code with
   **zero user setup** and **no VRAM contention** with the chat model (candle,
   off-VRAM).
2. Embedding-model provisioning is **provenance-sound** (pinned rev +
   compiled-in SHA-256 verify, or build-time bundle) — never a silent/unverified
   fetch.
3. Indexing is **off the turn's critical path**; first turn is not a felt stall;
   steady state re-embeds only changed files.
4. There is a **measured three-arm number** (off / FTS / embeddings) — the
   default is whatever *earned* it, stated honestly, not assumed.
5. Large-repo indexing is either complete or **visibly flagged as partial** —
   never silent partial context.
6. The feature **disables at runtime and cancels in-flight indexing** without a
   restart; the on-disk cache is purgeable and excludes denylisted/secret-shaped
   content.
7. Nothing about turning it on can fail a turn; the graceful-no-op floor
   remains the backstop.

## Critique log

This revision folded in an adversarial critique pass (3 lenses):

- **Factual:** corrected the one real error — the first draft claimed `dgx.rs`
  "already speaks" `POST /api/pull`; it does not (SSH `ollama pull` +
  `GET /api/tags` only). Resolved by making candle self-provision, which
  removes the need for any Ollama pull client. All other code claims verified
  accurate.
- **Inertness:** default-on embeddings is *not* justified by the engine
  existing/being tested (mechanically-works ≠ helps); a `code_search` tool
  already ships (push-vs-pull); BM25 beats embeddings on the dominant code
  query. ⇒ Phase 0 rewritten as a **three-arm ablation** with a real decision
  rule, and the FTS-default and tool-default outcomes made first-class.
- **Completeness:** first-turn indexing blocks synchronously ⇒ new **Phase
  0.5**; 400-file cap ships silent partial context ⇒ visible-truncation notice
  in Phase 3.4; on-disk cache persists **plaintext secrets** ⇒ Phase 3.2 secret
  hygiene; content-hash keying mislabels moved/duplicate content and misses
  model/chunker version ⇒ corrected keying + version-namespacing in Phase 3.1;
  no rollback story ⇒ the Rollback/kill section.
