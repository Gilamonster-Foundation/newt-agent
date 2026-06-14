# Design: coder-symbolic memory — a verbatim-accurate symbolic index that survives compression

**Status:** design note (no implementation). Workstream B of the
disclosure-at-three-scales agenda (plan: *Disclosure at three scales —
memory, coder-symbols, workflows*). **The symbol index has a second consumer:
it is the verify oracle for the S1 build-check gate (§6A) — one extraction
primitive, two consumers (disclosure + verification).**
**Crates touched (when built):** `newt-coder` (symbol extractor + `[SYMBOLS]`
render seam), `newt-core` (the `on_pre_compress` distillation hook; the verify
oracle the gate consults; optionally a §6 turn-record column).
**Related:** `#319` / PR `#321` (the summarization-induced-hallucination fix
and its re-read breadcrumb — `docs/notes/2026-06-13-summarization-induced-hallucination.md`);
the **second nemotron confabulation** (post-`#321`) root-caused on the
knowledge board and tracked as `#332` (umbrella remediation) / `#334`
(collaborative `/plan`) — §6A is B's contribution to that remediation;
Workstream A (`docs/design/progressive-disclosure-memory.md`, sibling note —
B is A's index *specialized for code* and reuses A's fetch tool);
the 18.4 compression pipeline (`newt-core/src/agentic/compress.rs`).

---

## TL;DR

A prose summary of a file is **not** the file. That is the `#319` root cause:
when compression replaces a verbatim `read_file` result with prose, the exact
signatures the coding model needs are gone, and the surviving prose *asserts*
the file is known — so the model fabricates a plausible signature instead of
re-reading (`docs/notes/2026-06-13-summarization-induced-hallucination.md` §3–4).

`#321` shipped the honest **floor**: a deterministic re-read breadcrumb
(`compress.rs::reread_breadcrumb`) that names the dropped files and tells the
model to re-read them. It converts a confident hallucination into a re-read,
but it costs a round and a model can ignore it (note §6, "What the fix is
*not*").

This note designs the **principled fix** the note flagged as the follow-up: a
compact, **verbatim-accurate symbolic index** of each file — signatures, type
definitions, imports (optionally a call/dependency graph) — that is cheap
enough to keep in the working set and *survives compression where prose loses
the signatures*. The signature `pub fn connect(&self, url: &str, timeout:
Duration) -> Result<Session, ConnErr>` is ~70 bytes; the file is kilobytes. We
keep the 70 bytes verbatim and let the model pull the full body on demand via
Workstream A's fetch (`read_file` already exists). This is Workstream A's
budgeted INDEX, made code-aware.

The principle generalizes the `#319` note's lesson: *never present a paraphrase
as if it were the file.* A symbol is not a paraphrase — it is a verbatim
fragment of the file, and a fragment the model can trust.

**Second consumer (added after the post-`#321` incident).** The *same* index
that retains signatures for disclosure is also a **verify oracle**: a query
"does the symbol this code references actually exist?" The second nemotron
confabulation wrote `from newt_core import classify, Caveats` when the real
surface is `from newt_agent.core import Router, Tier` — a *reference* to symbols
that do not exist. An index of real definitions answers that in microseconds,
with no build, and is the cheap tier of the S1 verify gate (§6A). The extractor
is therefore designed **general-first** (one tree-sitter spine, language adapters
derived from it — Rust and Python both), because the oracle must resolve
references in whatever language the model wrote.

---

## 1. What exists today

The coder path is **prose-only**; there is **zero** AST / LSP / symbol handling
anywhere in the tree.

- **File discovery** — `newt-coder/src/workspace_scan.rs`. Two-pass
  (`scan_workspace_for_files`): prefer files the task mentions
  (`filter_mentioned`), else every source file (`scan_all_source_files`).
  Language awareness is a flat extension allow-list
  (`workspace_scan::SCAN_EXTENSIONS` = `rs toml py js ts go java c h cpp hpp
  md`) used only to *include or exclude a file*. Nothing parses the contents.
- **Prompt rendering** — `newt-coder/src/prompt.rs::render_files_block`. Emits
  each file's **verbatim** contents under `FILE: <path>` / `END-FILE`
  separators, dropping files once the running block exceeds
  `prompt::DEFAULT_CONTEXT_CAP_CHARS` (32_000 chars, ~8K tokens) and logging a
  `tracing::warn!`. There is no symbol/header layer — when the cap is hit a
  whole file simply vanishes from the prompt, all-or-nothing.
- **The pre-compress hook** — `newt-core/src/memory.rs`,
  `MemoryProvider::on_pre_compress(&self, messages) -> String`. Called by
  `MemoryManager::on_pre_compress` **before** old messages are summarized away;
  each provider may return text to fold into the compression. Today only
  `Summarizing::on_pre_compress` overrides it, and only to re-emit the previous
  compaction summary (`memory.rs`, the `Summarizing` impl). **No provider
  extracts symbols here.** This is the unused-for-symbols seam.
- **The `#321` breadcrumb** — `newt-core/src/agentic/compress.rs::reread_breadcrumb`,
  wired into `summary_message`. Names files touched in the summarized middle and
  instructs a re-read. It is the deterministic floor B builds *above*: B keeps
  the signatures so the re-read is rare rather than merely instructed.

So the raw materials — a file walker, a verbatim render path with a cap seam,
and a pre-compression hook — are present. The gap is everything between "we
have the file's text" and "we have its symbols."

---

## 2. Thesis: a symbol index is the type-aware retention `#319` asked for

The `#319` note's §7 generalizable principles name the exact shape of this fix:

> **2. Compression is task-typed.** Q&A tolerates lossy summary; coding (and
> anything needing verbatim tokens — code, configs, IDs, quotes) does not.
>
> **1. Recency ≠ relevance.** … at minimum keep a recoverable pointer to what
> you evict.

A symbolic index is both: it is the *type-aware* retention (we keep code's
load-bearing tokens — signatures — verbatim, and discard only prose-summarizable
narration), and it is a *recoverable pointer* (the symbol names the file and its
shape; the full body is one `read_file` away).

Crucially, a symbol survives compression **without** triggering the `#319`
failure, because it is not a paraphrase. The hazard in `#319` is epistemic: the
harness *asserted knowledge the model did not have* (note §4). A verbatim
signature asserts only what is true — *this function has this signature* — and
the model can act on it correctly. A `[SYMBOLS]` block is labelled, accurate,
and small: it tells the model the shape it can rely on, and (via the breadcrumb
/ fetch) what it must re-read for bodies. That is the honest harness the note
demands, made dense instead of merely apologetic.

**When symbols beat verbatim, and when they don't.** The win is asymmetric in
file size. A 40-line module is ~1.5 KB — under the cap, keep it **verbatim**;
extracting symbols only loses information for no budget gain. A 1,200-line
service file is ~40 KB — it either blows `DEFAULT_CONTEXT_CAP_CHARS` (gets
dropped whole today) or eats the budget for every other file. There, a
~2 KB `[SYMBOLS]` block of its public surface is the difference between "the
model knows the API exists and its exact shape" and "the model never saw it."
The design must therefore treat verbatim-vs-symbols as a **budget decision**,
not a global mode (§4.4).

---

## 3. Extractor choice: tree-sitter (with a regex floor for the spike)

### 3.1 Recommendation

Use **tree-sitter** + a per-language grammar crate (`tree-sitter-rust` first)
for symbol extraction, with a **regex floor** as the spike's first increment so
the render/compress wiring can be proven before the parser dependency lands.

tree-sitter is the right tool: it is an incremental parser designed exactly for
"give me the declarations in this file" queries, it has a mature grammar
ecosystem (one crate per language), and its query language (`.scm` capture
patterns) expresses "capture every `function_item` name + parameters + return
type" declaratively — no hand-rolled Rust-syntax matcher.

### 3.2 The wheel / MSRV verdict (load-bearing — verified, not assumed)

The workspace pins MSRV at **1.75** (`Cargo.toml` `[workspace.package]
rust-version = "1.75"`) and follows a strict **pin-an-older-version**
discipline for deps whose latest release raised MSRV past the floor — `diffy`
is pinned to 0.4 ("diffy 0.5 raises its MSRV to 1.85"), `rusqlite` to 0.31
("0.32+ use post-1.75 language features"). tree-sitter must obey the same rule.

Verified against crates.io on 2026-06-13 (`cargo info`):

| crate | latest | latest MSRV | MSRV-resolved version | resolved MSRV |
|---|---|---|---|---|
| `tree-sitter` | 0.26.9 | **1.77** ✗ | **0.24.7** | **1.65** ✓ |
| `tree-sitter` | 0.25.0 | 1.76 ✗ | — | — |
| `tree-sitter-rust` | 0.24.2 | (unspecified) | 0.24.x | — |

So: **the latest tree-sitter (0.26) is above our 1.75 floor (MSRV 1.77), but
0.24.7 resolves at MSRV 1.65 and is well within range** — pin `tree-sitter =
"0.24"` exactly as `diffy = "0.4"` and `rusqlite = "0.31"` are pinned, with the
same one-line rationale comment in `[workspace.dependencies]`. Cargo's MSRV-aware
resolver already selects 0.24.7 here; the explicit pin makes it intentional and
documents *why* (a future contributor must not bump to 0.26 without first
raising the workspace MSRV).

**The "pure-Rust" claim needs one honest correction.** tree-sitter is *not*
strictly pure-Rust: the core runtime and every grammar ship a small **C** source
(the parser tables and an optional C scanner) compiled at build time via the
`cc` crate. There is no *system* dependency, no dynamic linking, and nothing
fetched at runtime — the C is **vendored and compiled from source**, exactly
like `rusqlite`'s `bundled` feature already does in this workspace
(`Cargo.toml`: "`bundled` compiles SQLite from source so newt has zero system
deps"). So the precise verdict is:

> tree-sitter is **wheel-safe in the same sense `rusqlite = { features =
> ["bundled"] }` is**: vendored C compiled from source, no system libs, no
> runtime fetch. It preserves the wheel story (maturin's manylinux build images
> already carry a C toolchain — they build `rusqlite` today) — but it is *not*
> the literally-pure-Rust "no C at all" that `diffy` is.

This nuance is the kind `#319` is about: state provenance accurately. The dep is
acceptable; calling it "pure Rust" would be the falsehood. If a literally-pure
extractor is ever required (e.g. a target with no C toolchain), the regex floor
(§3.3) and `syn` (a pure-Rust Rust parser, but Rust-only and heavier) are the
fallbacks — noted as open questions, not chosen here.

### 3.3 The regex floor (the spike's first step)

Before any parser dependency, the spike ships a **regex/line-based extractor for
Rust** that captures the high-value, high-regularity declarations:
`pub fn` / `fn` signatures, `pub struct` / `enum` / `trait` / `type` headers,
`impl` headers, and `use` lines. This is deliberately incomplete (it will miss
macro-generated items, multi-line generic bounds, etc.) but it is **zero new
dependencies** and proves the *render + compress wiring* — the part with the
most integration risk — independently of the parser choice. The golden test
(§7) is written against the regex floor first; tree-sitter must pass the *same*
golden plus the cases the regex floor punts on. The regex floor is the spike's
escape valve, not the destination.

---

## 4. Design

### 4.1 The symbol extractor

A new module in `newt-coder` (proposed `newt-coder/src/symbols.rs`) exposing,
roughly:

```rust
/// One extracted declaration, rendered verbatim from the source.
pub struct Symbol {
    pub kind: SymbolKind,   // Fn | Struct | Enum | Trait | TypeAlias | Impl | Import | Const
    pub signature: String,  // VERBATIM slice of the file — never paraphrased
    pub line: usize,        // 1-based, for the model to jump/re-read
}

/// Extract the symbolic surface of one file. `None` for languages with no
/// grammar wired yet → caller falls back to verbatim/prose (honest: we do not
/// invent symbols for a language we cannot parse).
pub fn extract_symbols(path: &Path, source: &str) -> Option<Vec<Symbol>>;
```

**Verbatim invariant (load-bearing).** A `Symbol::signature` is a *byte slice of
the source*, not a reconstruction. tree-sitter gives node byte ranges; we slice
`source[node.start_byte..node.end_byte]` (the signature span, body excluded).
This is what makes the symbol trustworthy under the `#319` lens — it cannot
drift from the file because it *is* the file's bytes. A test asserts the
extracted signature is a substring of the source (§7).

### 4.2 Where it renders: a `[SYMBOLS]` header in `render_files_block`

`prompt.rs::render_files_block` gains a per-file `[SYMBOLS]` header. The block
shape becomes:

```
FILE: src/api.rs
[SYMBOLS]
pub struct ApiClient { /* ... */ }          (L12)
pub fn connect(&self, url: &str, timeout: Duration) -> Result<Session, ConnErr>  (L34)
pub fn reconnect(&self) -> Result<(), ConnErr>  (L51)
use std::time::Duration;                     (L1)
[END-SYMBOLS]
<verbatim body, if it fits the budget>
END-FILE
```

The header is **cheap and always present**: even when the verbatim body is
dropped by the cap, the `[SYMBOLS]` block stays. This converts today's
all-or-nothing drop (§1) into graceful degradation: full body → symbols-only →
(only if even symbols overflow) a named omission line. The model always knows
the file *exists* and its *shape*; it pulls the body on demand.

**Reuse, do not duplicate, A's fetch.** The "pull the body on demand"
affordance is **Workstream A's fetch tool** (`recall_detail` / `memory_fetch`,
mirroring `use_skill`) over the already-existing `read_file` tool — B adds **no
new fetch tool**. The symbol header is the index entry; A's fetch is the
disclosure. B's only new surface is the *extractor* and the *render/compress
integration*; retrieval is A's.

The header must respect the pinned `WHOLE_FILE_SYSTEM_PROMPT` contract: the
system prompt tells the model to emit complete file bodies under
`FILE:`/`END-FILE`. The `[SYMBOLS]`…`[END-SYMBOLS]` block is *input context
only* and must be visibly distinct from the emit format so a weak local model
never echoes a symbol block back as a file body. (The exact markers are a spike
detail; the constraint is that they not collide with `FILE:`/`END-FILE` — the
bake-off showed this prompt's wording is load-bearing, `prompt.rs` doc comment.)

### 4.3 Compress integration: distill files to symbols in `on_pre_compress`

This is where the index *survives compression*. A new symbol-aware
`MemoryProvider` (or an extension of the coder's provider) overrides
`on_pre_compress`. When the loop is about to summarize the middle, the provider
walks `messages` for file-bearing turns (the same `read_file`/`edit_file`/
`write_file` results `reread_breadcrumb` already scans), extracts their symbols,
and returns a `[SYMBOLS]` block to fold into the retained context **alongside**
the prose summary.

The result: where today the middle becomes prose + a "re-read these files"
breadcrumb (`#321`), it becomes prose + the **actual verbatim signatures** of
those files. The breadcrumb stays as the floor for anything the extractor
couldn't parse (unknown language, parse failure) — symbols *augment* the
breadcrumb, they don't replace it. A model that follows the breadcrumb still
re-reads; a model that doesn't now has the signatures anyway. Defense in depth
for exactly the `#319` failure.

`on_pre_compress` returns a `String` today and `MemoryManager::on_pre_compress`
joins all providers' contributions — so this integration needs **no trait
change**: the symbol provider just returns a non-empty `[SYMBOLS]` string. (A
later refinement could make the symbol block structurally distinct in the
summary message; out of scope for the MVP.)

### 4.4 Budget decision: verbatim vs. symbols (not a global mode)

Per §2, the choice is per-file and budget-driven, made inside
`render_files_block` / the compress distillation:

1. If the file's verbatim body fits the remaining budget → keep it **verbatim**
   (symbols add nothing; small files lose information under extraction).
2. Else if its `[SYMBOLS]` block fits → keep **symbols-only** + a fetch pointer.
3. Else → a named omission line (`#321` breadcrumb shape) — the honest floor.

This makes the symbolic index a *budget-pressure* mechanism: under no pressure,
nothing changes (verbatim everywhere, today's behaviour); under pressure, files
shed their bodies to symbols before they shed their existence. It also keeps the
spike's blast radius small — under the default cap on small workspaces, behaviour
is byte-identical to today.

### 4.5 Optional: a symbolic column on the §6 turn record

The durable conversation store (`newt-core/src/store.rs`) records turns with the
17.6 token columns and an `encoding_version` discipline. A *future* refinement
could persist a turn's extracted `[SYMBOLS]` block as a dedicated column, so a
restored conversation rehydrates symbols without re-extracting. This is
explicitly **deferred**: it touches the store schema and its `encoding_version`
bump discipline (any column change must version the encoding so old rows decode),
and the MVP gets the full `#319` win without it (extraction is cheap and
in-process; re-extracting on restore is fine). Noted so the seam is known, not
built.

---

## 5. Languages: scope the spike to Rust; multi-language is incremental

The spike wires **one** language end-to-end: **Rust**, via `tree-sitter-rust`
(after the regex floor). Be honest that each additional language is its own
grammar crate, its own `.scm` query, and its own golden test — multi-language is
**incremental, not free**.

The eventual extension→grammar mapping (aligned with
`workspace_scan::SCAN_EXTENSIONS`):

| extension(s) | grammar crate | spike status |
|---|---|---|
| `rs` | `tree-sitter-rust` | **MVP** |
| `py` | `tree-sitter-python` | later |
| `js`, `ts` | `tree-sitter-javascript`, `tree-sitter-typescript` | later |
| `go` | `tree-sitter-go` | later |
| `java` | `tree-sitter-java` | later |
| `c`, `h`, `cpp`, `hpp` | `tree-sitter-c`, `tree-sitter-cpp` | later |
| `toml`, `md` | — (no symbol surface; verbatim/prose as today) | n/a |

`extract_symbols` returning `None` for an unwired extension is the honest
fallback: the file renders verbatim (or gets the prose+breadcrumb on compress)
exactly as today. We never fabricate symbols for a language we can't parse —
that would re-introduce the `#319` hazard.

---

## 6. Relationship to Workstream A (explicit)

| concern | Workstream A | Workstream B |
|---|---|---|
| the index | budgeted memory index (note titles, turn keywords, compaction markers) | per-file `[SYMBOLS]` block (signatures, types, imports) |
| the fetch | **`recall_detail` / `memory_fetch` tool** (new) | **reuses A's fetch** + the existing `read_file` — adds none |
| the principle | context is a budgeted, addressable resource | same, specialized to code's verbatim-token needs |

B is A's index made code-aware. The disclosure *mechanism* (a small index in the
working set; full content pulled on demand) is A's; B contributes the
code-specific *extractor* and the *render/compress* wiring. **B must not ship its
own fetch tool** — if A's fetch isn't merged when B's MVP lands, B's render path
points at the already-existing `read_file` tool (the breadcrumb already assumes
re-read via `read_file`), and adopts A's richer fetch when available. No
duplication.

---

## 6A. Second consumer: the symbol index as the S1 verify oracle

Workstream B was scoped as a *memory/disclosure* feature. The second nemotron
confabulation (post-`#321`, root-caused on the knowledge board, tracked in
`#332`) shows the same primitive answers a *verification* question, and the two
uses share one index. This section records that second consumer and the design
decisions taken with the operator on 2026-06-14.

### 6A.1 The reframe — extraction is the oracle

`#319` was a *signature* loss (the model forgot a real signature and fabricated
one). The new incident is a *reference* error: the model wrote
`from newt_core import classify, Caveats` / `from newt_data import DataStore`
against modules and names that **do not exist** — the real surface is one
umbrella module `newt_agent` with submodules (`newt_agent.core` exposing
`Router`/`Tier`, etc.). `python -m py_compile` — the check newt's own config
*recommends* for Python (`newt-core/src/config.rs:518-535`) — is **import-blind**
(it checks syntax, not import resolution) and caught 1 of the 5 broken files.

A symbol index of **real definitions** answers "does referenced symbol `X`
resolve?" directly. So the same `[SYMBOLS]` extraction that retains signatures
for disclosure is the **cheap, build-free tier** of the S1 verify gate: parse the
written file's references (imports / call targets), look each up in the
workspace's extracted definitions, and a miss is the confabulation signal. This
is the tier that catches the 4 files `py_compile` missed, in microseconds, with
no compiler invoked.

> **One primitive, two consumers.** Disclosure asks "what does this file
> *define*, compactly?" Verification asks "do the symbols this file *references*
> exist?" Both read the same definition index. B builds the index once; §4 is the
> disclosure consumer, §6A.3 is the verification consumer.

### 6A.2 General-first spine (the operator's call)

The extractor is built **general-first**: one tree-sitter spine (§3), with
per-language adapters *derived from it* — **not** a Python special case that Rust
is later retrofitted into. Rationale: the oracle must resolve references in
whatever language the model emitted, so a Rust example calling a fabricated Rust
symbol and a Python example importing a fabricated module are the *same* query
against the *same* index shape. Python and Rust are the first two adapters; both
descend from the general extractor. (This generalizes §5's "Rust-first, others
incremental" — the *extraction shape* is general from B0; the language *adapters*
land incrementally, Rust then Python, because the oracle's first regression test
is the Python incident and the workspace's own code is Rust.)

### 6A.3 Definition side vs. reference side

§4's `extract_symbols` is the **definition side**: what a file *declares*. The
oracle adds a thin **reference side**: extract a file's outbound references
(import statements, qualified call targets) and resolve them against the
workspace definition index. Two new pieces, both small and both on the same
tree-sitter spine:

- `extract_references(path, source) -> Vec<Reference>` — the imports/calls a file
  makes (mirror of `extract_symbols`).
- A workspace **definition index** (a map from fully-qualified symbol → defining
  `file:line`) built by running `extract_symbols` across the workspace once and
  caching it. A `resolve(reference) -> Resolved | NotFound` query is the oracle.

A `NotFound` on a reference the model just wrote is the fabrication signal. This
is strictly additive to §4 — same extractor, one more query.

### 6A.4 The FFI boundary — where static analysis can't see, and the fix

There is exactly one place a static tree-sitter index *alone* is insufficient:
the **FFI boundary**, where *what one language defines* ≠ *what another imports*.
The incident lives here. A `#[pyclass] struct Router` in Rust is projected into
Python as `newt_agent.core.Router` by the `#[pymodule]` / `m.add_class::<…>()`
macro (`newt-agent-py/src/lib.rs`); a tree-sitter scan of the Rust source sees
`struct Router` but **not** that it surfaces under `newt_agent.core`. So the
Python-visible surface cannot be derived from static Rust parsing without
modelling the PyO3 macro.

The fix is a second feeder into the same symbol store:

- **Static extractor** (tree-sitter) — the cheap, **build-free**, *same-language*
  oracle (Python→Python, Rust→Rust). Ships first; fits a CPU-constrained box.
- **FFI-introspection adapter** — a **build-once** pass: build the extension
  (maturin), `import newt_agent`, walk `dir()` across submodules, emit an **exact
  manifest** (`symbols.json`) of the real Python-visible surface. It is ground
  truth from the *actual built module*, which is the "harness stamps, model never
  asserts" rule applied to the symbol set: the manifest is the real surface, not a
  parse-time guess. The `#[pymodule]` registration is the authoritative source;
  runtime introspection is the exact reading of it.

One symbol store, two feeders (static + FFI-introspection). The general spine
handles the bulk; the adapter handles the single boundary static analysis can't
cross.

### 6A.5 Not-built vs. fabricated (the false-positive trap)

A naïve `import newt_agent` collapses two very different failures into one
`ModuleNotFoundError` (verified: `newt-agent-py/.../conftest.py` does a bare
import with no classification). The oracle must discriminate, in two stages:

1. **Built?** Probe the *umbrella* (`import newt_agent` / `_newt_agent*.so`
   presence). Failure → **not-built = environment error** ("run maturin"), *not*
   a model-confabulation signal and *not* a verify failure of the model's work.
   This stage kills the false positive.
2. **Resolve the name** (only if built) against the manifest / live import.
   Umbrella imports fine but `newt_agent.<X>` / `from <mod> import Y` does not
   resolve → **fabrication = the real signal**, fail the verify.

### 6A.6 What lives here vs. in the gate (`#332` S1)

This note owns the **oracle** (the index, the reference resolver, the FFI
adapter, the not-built/fabricated discriminator). The **gate mechanics** — where
the check fires (write-site cheap tier vs. subtask-close compile tier), how it
blocks (stage-verify-promote: write to temp, verify, promote-or-revert), the
`on_fail = "revert-retry"` (default) `| "keep-fix"` policy, and the honest
cap-exit failure banner — live in `#332` S1 (grounded at
`newt-core/src/agentic/tools.rs:393` advisory build-check, `:667` tool-result
contract, `:1146` `tool_result_ok`, and `mod.rs:1324`/`2033` cap-exit). The
division: **B provides the oracle; S1 enforces with it.** The cheap same-language
oracle is the build-free first increment that fits a constrained box; the FFI
adapter and the compile tier need a build and follow.

---

## 7. Test plan & acceptance

Deterministic, model-free — the `#319` discipline ("test compression
deterministically"; note §7.5). All under `newt-coder` + `newt-core` unit/e2e
suites; ≥80% coverage floor (`just cov-ci`).

1. **Golden symbol extraction.** A known fixture file (start with this repo's
   own `newt-coder/src/prompt.rs` or a purpose-built fixture) → assert the
   extracted `[SYMBOLS]` block contains the *real* signatures
   (`pub fn build_prompt(workspace: &Path, task: &str) -> Result<CoderPrompt>`,
   `pub const DEFAULT_CONTEXT_CAP_CHARS: usize`, the `pub struct CoderPrompt`
   fields) and **no** invented ones. Written against the regex floor first;
   tree-sitter must pass the same golden plus the cases the floor punts on.
2. **Verbatim invariant.** For every extracted `Symbol`, assert
   `source.contains(&symbol.signature)` — a signature that is not a byte-slice
   of the file fails the build. This is the property that makes the index
   trustworthy under `#319`.
3. **Compression preserves signatures (the `#319` probe, extended).** Take the
   existing regression guard
   (`compress.rs::tests::summarized_file_reads_get_a_reread_breadcrumb`, which
   today asserts `api_signature_survived=false` pre-fix and that the breadcrumb
   names the file) and add a sibling test asserting that **with the symbol
   provider wired, the verbatim `connect()` signature *does* survive** the same
   compression that drops the prose body. Same `#319 PROBE` shape; the new
   assertion is `api_signature_survived=true`.
4. **Budget degradation.** A file over `DEFAULT_CONTEXT_CAP_CHARS` →
   `render_files_block` emits its `[SYMBOLS]` header even though the verbatim
   body is dropped (today it vanishes entirely); a file under the cap →
   byte-identical to today (verbatim, no symbol churn).
5. **Honest fallback.** An unparseable / unwired-language file →
   `extract_symbols` returns `None` and the render path matches today's
   behaviour (no fabricated symbols).
6. **Verify oracle (§6A), model-free, against the incident.** Build a workspace
   definition index over a fixture mirroring the real surface
   (`newt_agent.core` defines `Router`/`Tier`); assert `resolve` returns
   `NotFound` for the fabricated references from the incident
   (`from newt_core import classify`, `from newt_data import DataStore`) and
   `Resolved` for the real ones (`from newt_agent.core import Router`). Assert
   the not-built vs. fabricated discriminator (§6A.5) classifies a missing
   umbrella as *environment* and a missing submodule name as *fabrication*. This
   is the deterministic regression test for the second nemotron incident.

**Acceptance (MVP):** the Rust extractor (regex floor → `tree-sitter-rust`) +
the `[SYMBOLS]` header in `render_files_block` + the `on_pre_compress`
distillation, with tests 1–5 green and the existing `#321` regression guard
still passing. Call-graph and multi-language are **deferred**.

### 7.1 Phasing

| phase | deliverable | scope |
|---|---|---|
| B0 (spike) | regex-floor Rust extractor + golden (test 1, 2) | no new deps; proves extraction shape |
| B1 (MVP) | `tree-sitter` (pinned `0.24`) + `tree-sitter-rust`; `[SYMBOLS]` header in `render_files_block`; budget degradation (tests 3, 4, 5) | the full `#319` win for Rust |
| Bv (verify oracle, §6A) | `extract_references` + workspace definition index + `resolve`; `tree-sitter-python` adapter; the build-once FFI-introspection manifest + not-built/fabricated discriminator (test 6) | the S1 cheap (build-free) tier; first regression = the second nemotron incident |
| B2 | `on_pre_compress` symbol distillation hardening; surface in `/memory` | continuity |
| B3+ (deferred) | call/dependency graph; additional languages; §6 store column | one grammar / feature each |

---

## 8. Open questions & risks

1. **Dep weight / wheel / MSRV (addressed, but watch).** Pin `tree-sitter =
   "0.24"` to stay ≤ MSRV 1.75 (verified §3.2); document the pin rationale like
   `diffy`/`rusqlite`. The honest caveat: it compiles vendored C via `cc`
   (wheel-safe like `rusqlite bundled`, *not* literally pure-Rust). Risk: a
   future workspace MSRV bump is the prerequisite for tracking tree-sitter past
   0.24. Mitigation: the regex floor keeps B functional with zero deps if the
   parser dep is ever rejected.
2. **Grammar-per-language cost.** Each language is a separate crate + query +
   golden. Multi-language is linear work; the spike is Rust-only on purpose
   (§5). Risk: scope creep into "support all of `SCAN_EXTENSIONS`" — explicitly
   out of scope for the MVP.
3. **Call-graph scope — probably defer.** A call/dependency graph is the most
   valuable *and* most expensive symbol artifact (it needs cross-file name
   resolution, which tree-sitter alone does not give — that's LSP territory).
   The MVP captures per-file declarations only; the call graph is B3+, gated on
   whether per-file symbols alone close `#319` in practice (they should — `#319`
   was a *signature* loss, not a *call-graph* loss).
4. **Keeping the index in sync as files change.** The extractor runs at
   render/compress time from the file's *current* bytes, so the in-prompt index
   is fresh by construction. The risk is the §6 store column (B3): a persisted
   symbol block can go stale if the file changed since the turn was recorded —
   which is *fine and correct*, because it describes the file *as it was at that
   turn* (provenance is the turn), but it must be labelled as such (the same
   `#319` discipline). Another reason B3 is deferred.
5. **When symbols beat verbatim, and when they don't (§2, §4.4).** Small files:
   keep verbatim — extraction only loses information. The budget decision must
   be per-file, never a global "always symbolize" mode, or the MVP regresses
   small-workspace behaviour.
6. **Emit-format collision.** The `[SYMBOLS]` block is input-only and must not
   be mistaken by a weak local model for the `FILE:`/`END-FILE` emit format
   (§4.2). The pinned `WHOLE_FILE_SYSTEM_PROMPT` is load-bearing per the
   bake-off; any marker choice that risks collision must be re-validated against
   the strategy bake-off, not changed casually.

---

## 9. Summary

`#319` proved that prose-summarizing a coding session induces hallucination
because a summary *asserts* knowledge it does not contain. `#321` made the
harness honest (re-read breadcrumb). This note designs the next layer: keep a
compact, **verbatim-accurate symbolic index** — signatures, types, imports —
that survives compression where prose loses the signatures, rendered as a cheap
`[SYMBOLS]` header and pulled to full content on demand via Workstream A's
fetch. The extractor is **tree-sitter** (pinned `0.24` for MSRV 1.75; wheel-safe
like `rusqlite bundled`, with an honest "vendored C, not pure-Rust" caveat),
spiked Rust-only behind a zero-dep regex floor, deferring the call-graph and
multi-language. The acceptance bar is the `#319` probe, extended:
*the verbatim signature survives the compression that drops the prose body.*
