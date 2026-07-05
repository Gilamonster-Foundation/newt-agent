# OllamAssist RAG — learnings for newt-agent

A source-read of [OllamAssist](https://github.com/BraisWaseem/OllamAssist)'s
RAG implementation (a JetBrains IDE plugin, `~/workspaces/OllamAssist`),
captured to inform newt-agent's own codebase-retrieval design. The companion
plan is [`design/rag-default-context-retrieval.md`](design/rag-default-context-retrieval.md).

**What OllamAssist is:** an IntelliJ plugin (Java 21, not Kotlin — the Kotlin
surface is only the `ProjectActivity` platform shim) that adds in-IDE chat,
autocomplete, and an agent mode over a local Ollama. Its headline feature is
"enhanced context awareness … through RAG." The RAG code lives under
`src/main/java/fr/baretto/ollamassist/chat/rag/`.

The point of reading it: OllamAssist and newt-agent are solving the *same*
problem (retrieve the parts of a codebase relevant to a query, inject them into
the model's context) from opposite ends — OllamAssist is a persistent,
on-disk, IDE-embedded index; newt-agent's `semantic.rs` is an in-memory,
per-session index. Each has made choices the other should learn from. This doc
is the honest ledger of both.

---

## The pipeline, end to end

| Stage | OllamAssist | Notable |
|---|---|---|
| Discover | recursive VFS walk from project root; include = path contains a settings substring (default `"src/"`); prune dot-dirs, IntelliJ-excluded/ignored roots, and git-ignored files (via `ChangeListManager`); skip binary + zero-length | `.gitignore`-aware through the IDE's VCS layer, not by parsing `.gitignore` |
| Cap | `RAGSettings.indexationSize` default **5000 files**, silent truncation past the cap with a `WARNING` toast | no sharding / per-module index |
| Chunk | `CodeAwareDocumentSplitter`: **Java-only** PSI structure-aware (one class-skeleton chunk + one chunk per method body); **everything else** → 60-line windows, 10-line overlap | 3000-char hard `substring` truncation on every chunk |
| Embed | DJL/ONNX `BgeSmallEnV15Quantized` (384-dim, fully local); falls back to Ollama `nomic-embed-text` if the native libs fail to load | dimensionality read from `embedding.vector().length`, never asserted |
| Store | custom **Apache Lucene** store on disk at `~/.ollamassist/<project>/database/knowledge_index/`; one segment set is *both* a KNN vector index and a BM25 text index | persisted across restarts; `INDEX_VERSION` string, wipe-and-rebuild on mismatch |
| Retrieve (chat) | `HybridRetriever`: KNN top-5 + BM25 top-5, merged by **Reciprocal Rank Fusion** (`k=60`), return top-3 | plus a live caret-window retriever that always runs |
| Inject | LangChain4j default content injector ("Answer using the following information …") | no custom prompt formatting |

---

## What's worth adopting

1. **Hybrid retrieval (KNN + BM25 + Reciprocal Rank Fusion).** This is the
   single best idea in OllamAssist and it is *specifically* right for source
   code. Pure vector similarity misses exact-identifier queries ("where is
   `parse_config` defined") that BM25 nails; BM25 misses semantically-similar-
   but-lexically-different code that KNN finds. RRF (`score = Σ 1/(k + rank)`,
   `k=60`) fuses two ranked lists without needing the score scales to be
   comparable. OllamAssist's own class docstring claims "+100–150% retrieval
   quality over KNN-only." newt-agent's `retrieve_evidence` is KNN-only today;
   it already has `DEF_BOOST`/`PATH_BOOST` reranking heuristics that gesture at
   the same intuition (a lexical/path match should win), so a BM25 arm is a
   natural, in-idiom upgrade.

2. **On-disk persistence keyed to survive restarts.** OllamAssist pays the
   embedding cost once and reuses the index across IDE sessions. newt-agent
   re-embeds the whole workspace on the first turn of *every* session. For a
   default-on feature this is the difference between "invisible" and "why is
   the first turn slow." (See the persistence caveat below — do it right.)

3. **A bundled local embedding model, so the feature can't silently no-op.**
   OllamAssist ships the BGE-small ONNX weights inside the plugin (DJL), so RAG
   works offline with zero setup. newt-agent already has the equivalent — the
   `CandleEmbedder` bge-small path in `newt-inference/src/embed.rs` — but it is
   opt-in behind `embeddings_api = "embedded"` and needs a model dir. A
   default-on feature must not depend on the user having run `ollama pull
   nomic-embed-text`.

4. **Class-skeleton chunks.** OllamAssist emits, per class, a "skeleton" chunk
   (fully-qualified name + fields + method *signatures*, bodies stripped)
   alongside the per-method body chunks. This gives retrieval an "outline" hit
   for "what does this type look like" without one giant chunk. newt-agent's
   chunker is def-boundary-based (good) but has no equivalent outline chunk;
   worth considering for large structs/impls.

---

## What to avoid (real bugs and naïveté found in the source)

1. **The stale-chunk invalidation bug — the most important cautionary tale.**
   On file change OllamAssist does delete-then-reinsert: `store.removeAll(new
   IdStartWithFilter(path))` before re-adding. But stored document IDs are
   `path + "/" + fileName + UUID.randomUUID()` (a random suffix), while
   `IdStartWithFilter.toLuceneQuery()` builds an **exact-match** `TermQuery` on
   the bare path — which can never equal an id with a random UUID glued on. So
   the delete matches nothing, and **every file edit silently leaves the old
   chunks in the index** alongside the new ones. Old and new versions of the
   same code stay retrievable indefinitely; only a full index wipe clears them.
   (Inferred from the Lucene API contract, not from running the plugin —
   "plausible," not "confirmed" — but the class has no test exercising the
   filter against a real stored id, which is itself the lesson.)
   **Design implication for us:** invalidation is the hardest part of a
   persistent code index, and an ID scheme that makes "delete all chunks for
   file X" a clean, testable operation is not optional. **Key chunks by content
   hash** — then a changed file's chunks are simply a different key, unchanged
   files skip re-embedding for free, and there is no delete-by-prefix footgun.

2. **Blind 3000-char truncation.** Every chunk is cut with a raw
   `substring(0, 3000)` — can slice a method mid-statement or mid-string.
   newt-agent already does better (split an oversized def into whole-line
   windows), and should keep doing better.

3. **Structure-awareness for exactly one language.** Despite running inside an
   IDE with PSI for dozens of languages, `CodeAwareDocumentSplitter` only
   special-cases `.java`; Kotlin, Python, JS, Markdown all get identical naïve
   60-line windows. **Lesson:** language coverage should be *data*, not a
   hardcoded `endsWith(".java")` branch. newt-agent's `LanguagePack` mechanism
   (`api_surface.rs`) is exactly the right shape for this — but note
   `gather_code_files` currently hardcodes `.rs`/`.py`, the same anti-pattern
   one layer up. Fixing that is part of the plan.

4. **Naïve include matching.** Inclusion is a substring `contains()` on the
   path, not a glob or `.gitignore`-syntax pattern — so `"src/"` also matches
   `/unrelated/src/tmp` and fails entirely on repos without a `src/` layout.
   Prefer a real ignore-walk (newt-agent already uses the `ignore` crate).

5. **A coarse 7-day project TTL as the only reconciliation.** Below the
   (broken) per-file invalidation, the *only* other freshness mechanism is
   "re-index the whole project if the registry entry is older than 7 days."
   There is no content-hash or mtime reconciliation pass. **Lesson:** a
   content-hash index makes reconciliation cheap and exact — walk the tree,
   hash each file, embed only the hashes you don't already have, drop the
   hashes for files that vanished.

6. **Dead code on the live path + a units bug.** The `minScore`/dynamic-
   threshold logic in `LuceneEmbeddingStore.search()` is fully implemented and
   unit-tested but the chat retriever bypasses it (calls `knnSearch`/`bm25Search`
   directly), so chat retrieval has *no* score floor. Separately,
   `ContextRetriever` documents a "2 second" timeout but the code passes
   `5000, TimeUnit.SECONDS` (~83 minutes). **Lesson:** the retrieval path that
   ships must be the path that's tested; a score floor that's implemented but
   unwired is worse than none, because the docs claim it exists.

7. **Off by default anyway.** For all the machinery, `RAGSettings.ragEnabled`
   defaults to `false`. The README's "boost accuracy through RAG" is opt-in.
   This is the same posture newt-agent has today — and the thing the plan
   proposes to change, *carefully*.

---

## The one thing OllamAssist gets structurally right that we don't yet

**It treats retrieval as a persistent, incrementally-maintained artifact of the
workspace, not a per-conversation scratch computation.** Its invalidation is
buggy and its chunking is Java-only, but the *shape* — a durable, on-disk,
content-addressed-ish index that survives restarts and updates on file change —
is the shape a default-on feature needs. newt-agent's `semantic.rs` has better
chunking, better failure handling, a cleaner embedder abstraction, and real
tests, but rebuilds from zero every session and never sees a mid-session edit.
The plan closes exactly that gap while keeping newt-agent's advantages.

---

## Appendix — key OllamAssist files

All under `src/main/java/fr/baretto/ollamassist/` unless noted:

- `chat/rag/FilesUtil.java`, `chat/rag/ShouldBeIndexed.java` — discovery, include/exclude, 5000-file cap
- `chat/rag/CodeAwareDocumentSplitter.java` — PSI Java chunking + line-window fallback
- `chat/rag/DocumentIngestFactory.java` — embedding-model selection (DJL BGE-small vs Ollama nomic-embed-text)
- `chat/rag/LuceneEmbeddingStore.java` — on-disk Lucene KNN+BM25 store, versioning, corruption recovery
- `chat/rag/HybridRetriever.java`, `chat/rag/RRFFusion.java` — chat-path retrieval + fusion
- `chat/rag/ContextRetriever.java`, `chat/rag/WorkspaceContextRetriever.java` — fan-out + caret-window
- `chat/rag/ProjectFileListener.java`, `chat/rag/Debouncer.java`, `chat/rag/IdStartWithFilter.java` — change → re-index wiring and the (buggy) invalidation
- `chat/rag/IndexRegistry.java` — per-project indexed/corrupted/TTL bookkeeping
- `completion/EnhancedContextProvider.java` — the *other* retrieval path (code completion, min-score-based)
- `agent/tools/rag/SearchKnowledgeBaseTool.java` — a third, BM25-only retrieval entry point for the agent mode
