//! Semantic repo-evidence retrieval — the `semantic` context feature (Step
//! 26.5, #582). Embedding RAG-for-code: chunk the repo, embed each chunk, and on
//! a query retrieve the most relevant code by cosine similarity, injected at the
//! head of the turn (gated by the `semantic` feature, like 26.3/26.4).
//!
//! **Step 26.5.1 — the embeddings client.** The [`Embedder`] trait is the seam
//! every downstream step (chunker indexing, retrieval) tests against with a
//! DETERMINISTIC mock — the real HTTP client never enters those tests, keeping
//! the whole subsystem in the fully-mocked unit tier. The real
//! [`EmbeddingsClient`] (Ollama `/api/embeddings`) is wiremock-tested here.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::Duration;

/// Turns text into an embedding vector. The mockable seam: indexing + retrieval
/// take `&dyn Embedder`, so they unit-test against a deterministic fake with
/// zero network. A genuine transport/backend failure is an `Err`.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Embed one text into a vector.
    async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>>;
}

/// The real [`Embedder`], protocol-aware over the two wire APIs newt speaks:
/// Ollama `POST /api/embeddings` (`{model, prompt}` → `{embedding: [f32]}`) and
/// OpenAI-compatible `POST /v1/embeddings` (`{model, input}` →
/// `{data: [{embedding: [f32]}]}`, e.g. vLLM serving an embedding model).
/// Mirrors the summarizer's HTTP discipline — a configurable timeout, optional
/// bearer auth, and exponential-backoff retry (embedding a whole repo is many
/// requests; transient failures recover).
pub struct EmbeddingsClient {
    url: String,
    model: String,
    kind: crate::BackendKind,
    api_key: Option<String>,
    timeout_secs: u64,
    retries: u32,
}

impl EmbeddingsClient {
    pub fn new(
        url: impl Into<String>,
        model: impl Into<String>,
        kind: crate::BackendKind,
        api_key: Option<String>,
        timeout_secs: u64,
        retries: u32,
    ) -> Self {
        Self {
            url: url.into(),
            model: model.into(),
            kind,
            api_key,
            timeout_secs,
            retries,
        }
    }

    /// Embed several texts (sequential — one request each, deterministic order).
    /// Fails fast on the first error so the index never holds a partial set.
    pub async fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            out.push(self.embed(t).await?);
        }
        Ok(out)
    }

    /// One embeddings request (no retry — the retry loop wraps this). The path,
    /// request shape, and response shape follow `self.kind`'s wire protocol.
    async fn embed_once(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let base = self.url.trim_end_matches('/');
        let (endpoint, body) = match self.kind {
            crate::BackendKind::Ollama => (
                format!("{base}/api/embeddings"),
                serde_json::json!({ "model": self.model, "prompt": text }),
            ),
            crate::BackendKind::Openai => (
                format!("{base}/v1/embeddings"),
                serde_json::json!({ "model": self.model, "input": text }),
            ),
            crate::BackendKind::Embedded => anyhow::bail!(
                "the embedded backend is chat-only and does not serve embeddings; \
                 set `embeddings_api` to an ollama/openai backend"
            ),
            crate::BackendKind::Anthropic => anyhow::bail!(
                "anthropic backends expose no embeddings surface; \
                 set `embeddings_api` to an ollama/openai backend"
            ),
        };
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(self.timeout_secs))
            .build()?;
        let mut req = client.post(&endpoint).json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("embeddings endpoint {endpoint} returned {}", resp.status());
        }
        let json: serde_json::Value = resp.json().await?;
        // Ollama: `{embedding: [..]}`; OpenAI: `{data: [{embedding: [..]}]}`.
        let arr = match self.kind {
            crate::BackendKind::Ollama => json["embedding"].as_array(),
            crate::BackendKind::Openai => json["data"][0]["embedding"].as_array(),
            // Unreachable: embedded and anthropic bail in the request match above.
            crate::BackendKind::Embedded | crate::BackendKind::Anthropic => None,
        }
        .ok_or_else(|| anyhow::anyhow!("embeddings response missing `embedding` array"))?;
        let vec: Vec<f32> = arr
            .iter()
            .map(|v| v.as_f64().unwrap_or(0.0) as f32)
            .collect();
        if vec.is_empty() {
            anyhow::bail!("embeddings response had an empty vector");
        }
        Ok(vec)
    }
}

#[async_trait]
impl Embedder for EmbeddingsClient {
    async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let mut last_err = None;
        for attempt in 0..=self.retries {
            if attempt > 0 {
                // Exponential backoff capped at ~4s: 250ms, 500ms, 1s, …
                let backoff = Duration::from_millis(250u64 << (attempt - 1).min(4));
                tokio::time::sleep(backoff).await;
            }
            match self.embed_once(text).await {
                Ok(v) => return Ok(v),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("embeddings failed")))
    }
}

// --- Step 26.5.2: code chunker (pure string → spans) ------------------------

/// One indexable unit of code: a definition (with its leading doc) or, when a
/// file has no recognized defs, a fixed line-window (Step 26.5.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeChunk {
    pub file: String,
    /// 1-based, inclusive.
    pub start_line: usize,
    /// 1-based, inclusive.
    pub end_line: usize,
    /// `function` / `struct` / … for a def, `window` for the fallback.
    pub kind: String,
    pub text: String,
}

/// Max chars per chunk; oversized def bodies split into line-windows.
pub const CHUNK_MAX_CHARS: usize = 2_000;
/// Line-window size for the no-defs fallback (and oversized splits).
const WINDOW_LINES: usize = 40;

/// Chunk `source` (the contents of `file`) into [`CodeChunk`]s (Step 26.5.2).
/// Pure: input strings, output Vec — the caller reads files. Reuses the
/// build-free `symbols::extract_definitions` to LOCATE defs, then slices the
/// span between consecutive defs (incl each def's leading doc/comment). A file
/// with no recognized defs (or an unknown language) falls back to fixed
/// line-windows so nothing is un-indexable.
pub fn chunk_source(file: &str, source: &str) -> Vec<CodeChunk> {
    let lines: Vec<&str> = source.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let mut defs = crate::symbols::Lang::from_path(file)
        .map(|lang| crate::symbols::extract_definitions(source, lang))
        .unwrap_or_default();
    if defs.is_empty() {
        return window_chunks(file, &lines, 1, lines.len());
    }
    defs.sort_by_key(|d| d.line);
    // Each def's block starts at its line, backed up over leading doc/comments.
    let starts: Vec<usize> = defs.iter().map(|d| block_start(&lines, d.line)).collect();
    let mut chunks = Vec::new();
    for (i, d) in defs.iter().enumerate() {
        let start = starts[i];
        let end = if i + 1 < defs.len() {
            starts[i + 1].saturating_sub(1).max(start)
        } else {
            lines.len()
        };
        let kind = format!("{:?}", d.kind).to_lowercase();
        let text = join_lines(&lines, start, end);
        if text.chars().count() > CHUNK_MAX_CHARS {
            // Oversized body → split into windows (so no chunk blows the budget).
            chunks.extend(window_chunks(file, &lines, start, end));
        } else {
            chunks.push(CodeChunk {
                file: file.to_string(),
                start_line: start,
                end_line: end,
                kind,
                text,
            });
        }
    }
    chunks
}

/// Walk up from `def_line` (1-based) over immediately-preceding comment/doc
/// lines to find where the def's block (incl its doc) starts.
fn block_start(lines: &[&str], def_line: usize) -> usize {
    let is_doc = |s: &str| {
        let t = s.trim_start();
        t.starts_with("///")
            || t.starts_with("//")
            || t.starts_with('#')
            || t.starts_with("\"\"\"")
            || t.starts_with("/*")
            || t.starts_with('*')
    };
    let mut start = def_line;
    while start > 1 && is_doc(lines[start - 2]) {
        start -= 1;
    }
    start
}

fn join_lines(lines: &[&str], start: usize, end: usize) -> String {
    let end = end.min(lines.len());
    if start > end {
        return String::new();
    }
    lines[start - 1..end].join("\n")
}

/// Fixed line-window chunks over `[from, to]` (1-based, inclusive).
fn window_chunks(file: &str, lines: &[&str], from: usize, to: usize) -> Vec<CodeChunk> {
    let mut chunks = Vec::new();
    let mut s = from;
    while s <= to {
        let e = (s + WINDOW_LINES - 1).min(to);
        chunks.push(CodeChunk {
            file: file.to_string(),
            start_line: s,
            end_line: e,
            kind: "window".to_string(),
            text: join_lines(lines, s, e),
        });
        s = e + 1;
    }
    chunks
}

// --- Step 26.5.3: in-memory vector store + cosine top-k retrieval -----------

/// Whole-block char cap for the injected `<code_evidence>` (Step 26.5.3) — the
/// budget guard, mirroring scratchpad's `STATE_TOTAL_CAP`.
pub(crate) const CODE_EVIDENCE_CAP: usize = 6_000;

/// A vector index over [`CodeChunk`]s (Step 26.5.3). `&self` interior mutability
/// so a single shared `&dyn SemanticIndex` serves the indexing + retrieval paths.
pub trait SemanticIndex: Send + Sync {
    /// Add an embedded chunk to the index.
    fn index_chunk(&self, chunk: CodeChunk, embedding: Vec<f32>);
    /// Top-`k` chunks by cosine similarity to `query`, highest score first.
    fn search(&self, query: &[f32], top_k: usize) -> Vec<(f32, CodeChunk)>;
    /// Chunks held (for `/context stats`).
    fn chunks_indexed(&self) -> u64;
    /// Total chars of indexed chunk text (for `/context stats`).
    fn indexed_chars(&self) -> u64;
    /// Drop the whole index (`/new`, or a re-index).
    fn clear(&self);
}

/// In-memory, session-scoped [`SemanticIndex`] — pure (no fs), discarded at
/// `/new`. A flat `Vec` + brute-force cosine: simple, deterministic, and plenty
/// for a single repo's chunks (no ANN/vector-db dependency in v1).
#[derive(Default)]
pub struct SessionSemanticIndex {
    entries: Mutex<Vec<(CodeChunk, Vec<f32>)>>,
}

impl SemanticIndex for SessionSemanticIndex {
    fn index_chunk(&self, chunk: CodeChunk, embedding: Vec<f32>) {
        self.entries.lock().unwrap().push((chunk, embedding));
    }
    fn search(&self, query: &[f32], top_k: usize) -> Vec<(f32, CodeChunk)> {
        let entries = self.entries.lock().unwrap();
        let mut scored: Vec<(f32, CodeChunk)> = entries
            .iter()
            .map(|(c, e)| (cosine(query, e), c.clone()))
            .collect();
        // Descending by score; a stable sort keeps index order for exact ties.
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(top_k);
        scored
    }
    fn chunks_indexed(&self) -> u64 {
        self.entries.lock().unwrap().len() as u64
    }
    fn indexed_chars(&self) -> u64 {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .map(|(c, _)| c.text.chars().count() as u64)
            .sum()
    }
    fn clear(&self) {
        self.entries.lock().unwrap().clear();
    }
}

/// Cosine similarity. Returns 0 for a dimension mismatch, an empty vector, or a
/// zero vector — a defensive default (orthogonal), NEVER a panic or NaN.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

// --- Step 26.5.4: indexing + retrieval (Embedder-driven, mockable) ----------

/// Index a set of `(file, source)` pairs (Step 26.5.4): chunk each file and
/// embed each chunk via the injected [`Embedder`], populating `index`. Returns
/// the count indexed. Best-effort — an embed failure SKIPS that chunk (the rest
/// still index), so a flaky/absent embedder degrades to fewer-or-no results
/// rather than aborting. fs/net-free here: the caller supplies the files and the
/// `Embedder` is the seam (tests inject a deterministic fake).
pub async fn index_files(
    files: &[(String, String)],
    embedder: &dyn Embedder,
    index: &dyn SemanticIndex,
    on_failure: crate::OnEmbedFailure,
) -> usize {
    let mut indexed = 0;
    for (file, source) in files {
        for chunk in chunk_source(file, source) {
            match embedder.embed(&chunk.text).await {
                Ok(v) => {
                    index.index_chunk(chunk, v);
                    indexed += 1;
                }
                Err(e) => match on_failure {
                    // A structural failure (wrong endpoint / missing model) is
                    // total, not transient — degrading per-chunk would silently
                    // build an empty index. Stop once with an actionable error.
                    crate::OnEmbedFailure::Disable => {
                        tracing::error!(
                            error = %e,
                            file = file.as_str(),
                            "semantic indexing disabled: embeddings failed. Configure \
                             [context.semantic] for a working embedder: use \
                             embeddings_endpoint/embeddings_api for an Ollama or OpenAI \
                             embeddings service, or embeddings_api = \"embedded\" with \
                             embedding_model_path for local in-process embeddings. Set \
                             on_embed_failure = \"warn\" to keep trying per-chunk. Indexed \
                             {indexed} chunk(s) before stopping."
                        );
                        return indexed;
                    }
                    crate::OnEmbedFailure::Warn => {
                        tracing::warn!(error = %e, file = file.as_str(), "embed failed; skipping chunk");
                    }
                },
            }
        }
    }
    indexed
}

// --- Step 26.5.6: rerank (cheap, deterministic re-scoring) ------------------

/// Over-fetch factor: retrieval pulls `top_k * RERANK_OVERFETCH` cosine
/// candidates so the rerank can promote a slightly-lower-cosine but
/// structurally-better chunk into the final top_k.
const RERANK_OVERFETCH: usize = 3;
/// A real definition outranks a raw line-window at near-equal similarity.
const DEF_BOOST: f32 = 0.05;
/// A chunk whose file path contains a query term is nudged up.
const PATH_BOOST: f32 = 0.05;

/// Evidence provenance label (#1387). Semantic similarity is never structural
/// proof — callers must surface the kind to both human and model consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    Lexical,
    Symbol,
    Graph,
    Semantic,
    Curated,
}

impl EvidenceKind {
    /// Bracket label for human/model surfaces (`[SEMANTIC]`, …).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Lexical => "[LEXICAL]",
            Self::Symbol => "[SYMBOL]",
            Self::Graph => "[GRAPH]",
            Self::Semantic => "[SEMANTIC]",
            Self::Curated => "[CURATED]",
        }
    }
}

/// One ranked retrieval candidate with decomposed scores (#1387 Phase 1).
#[derive(Debug, Clone, PartialEq)]
pub struct RankedHit {
    pub chunk: CodeChunk,
    pub kind: EvidenceKind,
    pub cosine: f32,
    pub def_boost: f32,
    pub path_boost: f32,
    pub final_score: f32,
}

impl RankedHit {
    /// Stable location key (`file:start-end`) for pin/exclude identity.
    #[must_use]
    pub fn loc_key(&self) -> String {
        format!(
            "{}:{}-{}",
            self.chunk.file, self.chunk.start_line, self.chunk.end_line
        )
    }
}

/// Why a candidate did not enter the model-facing evidence packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// Ranked below the selected top_k.
    BelowTopK,
    /// Did not fit the `<code_evidence>` char budget.
    BudgetExhausted,
    /// Operator (or path) exclusion.
    Excluded,
}

/// Structured retrieval outcome — string render is a view over this (#1387).
#[derive(Debug, Clone, PartialEq)]
pub struct RetrievalResult {
    pub hits: Vec<RankedHit>,
    pub rejected: Vec<(RankedHit, RejectReason)>,
    /// Cosine candidates considered before top_k / budget cuts.
    pub candidates: usize,
    /// `true` iff the gather recorded no cuts (`GatherManifest.cuts` empty).
    pub complete: bool,
    /// Lightweight index identity (`genN:<hash-prefix>`).
    pub index_id: String,
    pub warnings: Vec<String>,
}

/// Session-scoped operator steering for retrieval (#1387). Cleared on `/new`.
#[derive(Debug, Clone, Default)]
pub struct RetrievalSteer {
    /// Hits forced into the next evidence packet (by loc key).
    pub pinned: Vec<RankedHit>,
    /// Path prefixes excluded from automatic retrieval.
    pub excluded_paths: Vec<String>,
}

impl RetrievalSteer {
    pub fn clear(&mut self) {
        self.pinned.clear();
        self.excluded_paths.clear();
    }

    /// True when `path` matches an exclusion (exact or prefix `excl/` / `excl`).
    #[must_use]
    pub fn is_excluded(&self, path: &str) -> bool {
        self.excluded_paths
            .iter()
            .any(|ex| path_is_excluded(path, ex))
    }

    pub fn pin(&mut self, hit: RankedHit) {
        let key = hit.loc_key();
        self.pinned.retain(|h| h.loc_key() != key);
        self.pinned.push(hit);
    }

    pub fn exclude_path(&mut self, path: impl Into<String>) {
        let path = path.into();
        if !self.excluded_paths.iter().any(|p| p == &path) {
            self.excluded_paths.push(path);
        }
        let excluded = self.excluded_paths.clone();
        self.pinned.retain(|h| {
            !excluded
                .iter()
                .any(|ex| path_is_excluded(&h.chunk.file, ex))
        });
    }
}

fn path_is_excluded(path: &str, ex: &str) -> bool {
    path == ex || path.starts_with(&format!("{ex}/")) || path.starts_with(&format!("{ex}\\"))
}

/// Lightweight session index status (#1387) — not the durable `#1282` index.
#[derive(Debug, Clone, Default)]
pub struct IndexStatus {
    /// Bumped each time this session re-indexes.
    pub generation: u64,
    pub manifest: Option<GatherManifest>,
    pub git_head: Option<String>,
    pub dirty: Option<bool>,
}

impl IndexStatus {
    #[must_use]
    pub fn index_id(&self) -> String {
        match &self.manifest {
            Some(m) if m.candidate_hash.len() >= 8 => {
                format!("gen{}:{}", self.generation, &m.candidate_hash[..8])
            }
            Some(m) => format!("gen{}:{}", self.generation, m.candidate_hash),
            None => format!("gen{}", self.generation),
        }
    }

    /// Completeness from the gather: no cuts ⇒ complete.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.manifest
            .as_ref()
            .map(|m| m.cuts.is_empty())
            .unwrap_or(true)
    }
}

fn query_terms(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 3)
        .map(str::to_lowercase)
        .collect()
}

fn boosts_for(terms: &[String], chunk: &CodeChunk) -> (f32, f32) {
    let def_boost = if chunk.kind != "window" {
        DEF_BOOST
    } else {
        0.0
    };
    let file_lc = chunk.file.to_lowercase();
    let path_boost = if terms.iter().any(|t| file_lc.contains(t.as_str())) {
        PATH_BOOST
    } else {
        0.0
    };
    (def_boost, path_boost)
}

/// Turn cosine hits into [`RankedHit`]s with decomposed boosts, sorted by
/// `final_score` descending (stable on ties).
fn rank_hits(query: &str, hits: Vec<(f32, CodeChunk)>) -> Vec<RankedHit> {
    let terms = query_terms(query);
    let mut ranked: Vec<RankedHit> = hits
        .into_iter()
        .map(|(cosine, chunk)| {
            let (def_boost, path_boost) = boosts_for(&terms, &chunk);
            RankedHit {
                chunk,
                kind: EvidenceKind::Semantic,
                cosine,
                def_boost,
                path_boost,
                final_score: cosine + def_boost + path_boost,
            }
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.final_score
            .partial_cmp(&a.final_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    ranked
}

/// Re-score cosine `hits` with cheap, deterministic boosts (Step 26.5.6).
/// Test/legacy adapter over [`rank_hits`].
#[cfg(test)]
fn rerank(query: &str, hits: &mut [(f32, CodeChunk)]) {
    let ranked = rank_hits(query, hits.to_vec());
    for (slot, hit) in hits.iter_mut().zip(ranked) {
        *slot = (hit.cosine, hit.chunk);
    }
}

fn format_hit_piece(hit: &RankedHit) -> String {
    format!(
        "// {} {}:{}-{} ({}, score {:.2})\n{}\n\n",
        hit.kind.label(),
        hit.chunk.file,
        hit.chunk.start_line,
        hit.chunk.end_line,
        hit.chunk.kind,
        hit.final_score,
        hit.chunk.text
    )
}

const SEMANTIC_EVIDENCE_NOTE: &str =
    "// NOTE: SEMANTIC evidence is embedding similarity — not proof of a \
         call, reference, implementation, or reachability relationship.\n";

/// Apply the char budget to `selected`, moving overflow into `rejected` as
/// [`RejectReason::BudgetExhausted`]. Returns the hits that fit.
fn apply_budget(
    selected: Vec<RankedHit>,
    rejected: &mut Vec<(RankedHit, RejectReason)>,
    total_cap: usize,
) -> Vec<RankedHit> {
    let mut kept = Vec::new();
    let mut body_chars = "<code_evidence>\n".chars().count()
        + SEMANTIC_EVIDENCE_NOTE.chars().count()
        + "</code_evidence>".chars().count();
    let mut budget_hit = false;
    for hit in selected {
        let piece_chars = format_hit_piece(&hit).chars().count();
        if budget_hit || body_chars + piece_chars > total_cap {
            rejected.push((hit, RejectReason::BudgetExhausted));
            budget_hit = true;
            continue;
        }
        body_chars += piece_chars;
        kept.push(hit);
    }
    kept
}

/// Render a `<code_evidence>` block from a structured [`RetrievalResult`].
/// `None` when there are no selected hits (OFF/empty bit-for-bit guarantee).
pub fn render_code_evidence(result: &RetrievalResult) -> Option<String> {
    if result.hits.is_empty() {
        return None;
    }
    let mut body = String::from("<code_evidence>\n");
    body.push_str(SEMANTIC_EVIDENCE_NOTE);
    for hit in &result.hits {
        body.push_str(&format_hit_piece(hit));
    }
    body.push_str("</code_evidence>");
    Some(body)
}

/// Structured retrieval (#1387 Phase 1): embed → over-fetch → rank with
/// boosts → apply pin/exclude → top_k + budget. `None` when the query can't
/// embed or the index has nothing to score.
pub async fn retrieve_ranked(
    query: &str,
    embedder: &dyn Embedder,
    index: &dyn SemanticIndex,
    top_k: usize,
    steer: Option<&RetrievalSteer>,
    status: Option<&IndexStatus>,
) -> Option<RetrievalResult> {
    retrieve_ranked_with_cap(
        query,
        embedder,
        index,
        top_k,
        CODE_EVIDENCE_CAP,
        steer,
        status,
    )
    .await
}

/// Like [`retrieve_ranked`] with an explicit char budget (tests + tooling).
pub async fn retrieve_ranked_with_cap(
    query: &str,
    embedder: &dyn Embedder,
    index: &dyn SemanticIndex,
    top_k: usize,
    total_cap: usize,
    steer: Option<&RetrievalSteer>,
    status: Option<&IndexStatus>,
) -> Option<RetrievalResult> {
    let qv = embedder.embed(query).await.ok()?;
    let raw = index.search(&qv, top_k.saturating_mul(RERANK_OVERFETCH).max(top_k));
    if raw.is_empty() && steer.map(|s| s.pinned.is_empty()).unwrap_or(true) {
        return None;
    }
    let candidates = raw.len();
    let ranked = rank_hits(query, raw);
    let mut rejected = Vec::new();
    let mut eligible = Vec::new();
    for hit in ranked {
        if steer.is_some_and(|s| s.is_excluded(&hit.chunk.file)) {
            rejected.push((hit, RejectReason::Excluded));
        } else {
            eligible.push(hit);
        }
    }

    // Automatic top_k by score, then force-union operator pins.
    let mut selected: Vec<RankedHit> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for hit in eligible {
        let key = hit.loc_key();
        if selected.len() < top_k {
            seen.insert(key);
            selected.push(hit);
        } else {
            rejected.push((hit, RejectReason::BelowTopK));
        }
    }
    if let Some(steer) = steer {
        for pin in steer.pinned.iter().rev() {
            if steer.is_excluded(&pin.chunk.file) {
                continue;
            }
            let key = pin.loc_key();
            if seen.insert(key.clone()) {
                // Pinned hits are forced in even when they missed top_k.
                rejected.retain(|(h, _)| h.loc_key() != key);
                selected.insert(0, pin.clone());
            }
        }
    }

    let hits = apply_budget(selected, &mut rejected, total_cap);
    let complete = status.map(IndexStatus::complete).unwrap_or(true);
    let index_id = status
        .map(IndexStatus::index_id)
        .unwrap_or_else(|| "gen0".to_string());
    let mut warnings = Vec::new();
    if !complete {
        warnings.push("index incomplete: gather caps cut one or more candidate files".to_string());
    }
    warnings.push(
        "results are SEMANTIC evidence (embedding similarity), not structural proof".to_string(),
    );
    Some(RetrievalResult {
        hits,
        rejected,
        candidates,
        complete,
        index_id,
        warnings,
    })
}

/// Retrieve a reranked `<code_evidence>` block for `query` (Step 26.5.4 +
/// 26.5.6 + #1387): thin wrapper over [`retrieve_ranked`] →
/// [`render_code_evidence`]. `None` when the query can't embed, the index is
/// empty, or nothing matches — so an absent embedding model is a silent no-op,
/// not a turn failure.
pub async fn retrieve_evidence(
    query: &str,
    embedder: &dyn Embedder,
    index: &dyn SemanticIndex,
    top_k: usize,
) -> Option<String> {
    retrieve_evidence_steered(query, embedder, index, top_k, None, None).await
}

/// Like [`retrieve_evidence`] with session pin/exclude + index status (#1387).
pub async fn retrieve_evidence_steered(
    query: &str,
    embedder: &dyn Embedder,
    index: &dyn SemanticIndex,
    top_k: usize,
    steer: Option<&RetrievalSteer>,
    status: Option<&IndexStatus>,
) -> Option<String> {
    let result = retrieve_ranked(query, embedder, index, top_k, steer, status).await?;
    render_code_evidence(&result)
}

/// Walk `workspace` for indexable code files (Step 26.5.4) — gitignore-aware,
/// `.rs`/`.py` only (what the chunker understands), bounded for responsiveness.
/// Returns `(relative-path, source)` pairs. **Runtime fs glue** (NOT unit-tier:
/// it reads the real filesystem); the pure chunk/embed/index logic it feeds is
/// the fully-mocked part above. Reuses the `ignore` crate (newt-core's `find`
/// tool already depends on it).
/// The gather caps, lifted out of silent consts (#1281 / spec PR-0 §5.0). The
/// scan floor everything (the API surface, the embedding chunker, the project
/// model) sits on: a **degradation curve measured over a silently corrupted
/// gather pins nothing**, so the caps are *declared* and reported in the
/// [`GatherManifest`], not hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatherCaps {
    /// Max files kept (in lexicographic order). Default 400.
    pub max_files: usize,
    /// Max bytes per file; larger files are cut, not silently skipped. Default 200_000.
    pub max_bytes: u64,
}

impl Default for GatherCaps {
    fn default() -> Self {
        Self {
            max_files: 400,
            max_bytes: 200_000,
        }
    }
}

/// Why a candidate was cut from the gather.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CutClass {
    /// Larger than [`GatherCaps::max_bytes`].
    TooLarge,
    /// Beyond [`GatherCaps::max_files`] in lexicographic order.
    OverFileCap,
}

/// One candidate the caps dropped — the honest record (path + why).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cut {
    pub path: String,
    pub class: CutClass,
}

/// The manifest of a gather (#1281 / spec WF-4): a hash over the **full
/// candidate walk** (so a re-gather over the same tree is provably identical —
/// the double-gather vector), the declared caps, and the cut list. A silently
/// order-unstable or truncated gather can no longer masquerade as complete.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GatherManifest {
    /// Files that matched the extension allow-list (before caps).
    pub candidate_count: usize,
    /// blake3 hex over the sorted candidate paths — the full-walk identity.
    pub candidate_hash: String,
    pub max_files: usize,
    pub max_bytes: u64,
    /// The dropped candidates, in lexicographic order.
    pub cuts: Vec<Cut>,
}

impl GatherManifest {
    /// Per-top-dir rollup of cuts — the operator-facing "crate X lost N files"
    /// honesty line (spec PR-0). Groups by the first path segment; sorted.
    #[must_use]
    pub fn cut_rollup(&self) -> Vec<(String, usize)> {
        use std::collections::BTreeMap;
        let mut by_dir: BTreeMap<String, usize> = BTreeMap::new();
        for cut in &self.cuts {
            let top = cut
                .path
                .split(['/', '\\'])
                .find(|s| !s.is_empty())
                .unwrap_or(".")
                .to_string();
            *by_dir.entry(top).or_default() += 1;
        }
        by_dir.into_iter().collect()
    }
}

/// **Pure** gather planner (#1281 / WF-4): given the candidate `(path, size)`
/// list and caps, produce the KEPT paths **deterministically** — lexicographic
/// sort THEN cap — plus the [`GatherManifest`] (full-walk hash + cut list).
///
/// Sorting *before* the cap is the fix: the `ignore` crate's walk order is not
/// stable, so the old `break at MAX_FILES` kept a different 400 files each run,
/// and every downstream artifact was built on a silently different gather. A
/// too-large file is cut (`TooLarge`) and does not consume the file budget.
#[must_use]
pub fn plan_gather(
    candidates: &[(String, u64)],
    caps: GatherCaps,
) -> (Vec<String>, GatherManifest) {
    let mut cands: Vec<(String, u64)> = candidates.to_vec();
    cands.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = blake3::Hasher::new();
    for (path, _) in &cands {
        hasher.update(path.as_bytes());
        hasher.update(b"\n");
    }
    let candidate_hash = hasher.finalize().to_hex().to_string();

    let mut kept = Vec::new();
    let mut cuts = Vec::new();
    for (path, size) in &cands {
        if *size > caps.max_bytes {
            cuts.push(Cut {
                path: path.clone(),
                class: CutClass::TooLarge,
            });
        } else if kept.len() >= caps.max_files {
            cuts.push(Cut {
                path: path.clone(),
                class: CutClass::OverFileCap,
            });
        } else {
            kept.push(path.clone());
        }
    }
    (
        kept,
        GatherManifest {
            candidate_count: cands.len(),
            candidate_hash,
            max_files: caps.max_files,
            max_bytes: caps.max_bytes,
            cuts,
        },
    )
}

/// Gather source files whose extension is in `extensions`, honestly (#1281):
/// walk → collect readable candidates with sizes → [`plan_gather`] (sort + cap)
/// → read the kept files, returning `(files, manifest)`. The manifest records
/// admitted candidate paths and what the caps dropped. On Linux, metadata and
/// content reads resolve beneath one opened workspace capability.
///
/// The extension allow-list is a **parameter**, not a hardcoded `rs`/`py`
/// literal (#956): the API-surface caller derives it from the *resolved language
/// packs*; the embedding index passes its own narrower set (blast radius). An
/// empty `extensions` gathers nothing.
#[must_use]
pub fn gather_with_manifest(
    workspace: &str,
    extensions: &[String],
    caps: GatherCaps,
) -> (Vec<(String, String)>, GatherManifest) {
    use std::io::Read;

    let root = std::path::Path::new(workspace);
    #[cfg(target_os = "linux")]
    let directory = match crate::fs_cap::WorkspaceDir::open_root(root) {
        Ok(directory) => directory,
        Err(_) => return (Vec::new(), plan_gather(&[], caps).1),
    };
    let open = |relative: &std::path::Path| {
        #[cfg(target_os = "linux")]
        {
            directory.open(relative)
        }
        #[cfg(not(target_os = "linux"))]
        {
            std::fs::File::open(root.join(relative))
        }
    };
    let mut candidates: Vec<(String, u64)> = Vec::new();
    for entry in ignore::WalkBuilder::new(workspace).build().flatten() {
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str());
        if !ext.is_some_and(|e| extensions.iter().any(|x| x == e)) {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let Ok(metadata) = open(relative).and_then(|file| file.metadata()) else {
            continue;
        };
        candidates.push((relative.to_string_lossy().into_owned(), metadata.len()));
    }
    let (kept, manifest) = plan_gather(&candidates, caps);
    let mut files = Vec::with_capacity(kept.len());
    for rel in &kept {
        if let Ok(src) = open(std::path::Path::new(rel)).and_then(|mut file| {
            let mut source = String::new();
            file.read_to_string(&mut source)?;
            Ok(source)
        }) {
            files.push((rel.clone(), src));
        }
    }
    (files, manifest)
}

/// Deterministic gather (default caps), returning just the files — the stable
/// entry point for the API surface + embedding index. See [`gather_with_manifest`]
/// for the manifest (#1281). Now sorted, so a re-gather is reproducible.
#[must_use]
pub fn gather_code_files(workspace: &str, extensions: &[String]) -> Vec<(String, String)> {
    gather_with_manifest(workspace, extensions, GatherCaps::default()).0
}

// --- Step 26.5.5: the code_search tool (model-callable retrieval) -----------

/// The semantic searcher handed to the `code_search` tool (Step 26.5.5): an
/// embedder + the session index + the default top_k, bundled into ONE `ChatCtx`
/// field (both members are shared refs, so this is `Copy`). Optional steer /
/// index status (#1387) apply the same pin/exclude + completeness as auto-inject.
#[derive(Clone, Copy)]
pub struct CodeSearch<'a> {
    pub embedder: &'a dyn Embedder,
    pub index: &'a dyn SemanticIndex,
    pub top_k: usize,
    pub steer: Option<&'a RetrievalSteer>,
    pub status: Option<&'a IndexStatus>,
}

/// The `code_search` tool definition (Step 26.5.5) — advertised only when the
/// `semantic` feature is on and an index is present.
pub fn code_search_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "code_search",
            "description": "Search the indexed codebase for the most relevant code by MEANING \
                            (semantic/embedding search, not keyword) — use it to find where \
                            something is implemented when you don't have a file path, e.g. \
                            'where is the retry backoff computed'. Returns the top matching \
                            code chunks with their file:line; then read_file the ones you need. \
                            Results are SEMANTIC evidence (similarity), not proof of calls, \
                            references, or implementations.",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "What to find, in natural language or a symbol name." }
                },
                "required": ["query"]
            }
        }
    })
}

/// Execute a `code_search` call (Step 26.5.5 / #1387): structured retrieve →
/// render the matching `<code_evidence>` (or a labelled no-match).
pub(crate) async fn execute_code_search(
    args: &serde_json::Value,
    search: CodeSearch<'_>,
    _color: bool,
    _tool_output_lines: usize,
) -> String {
    let query = args["query"].as_str().unwrap_or("").trim();
    if query.is_empty() {
        return "error: code_search requires a non-empty `query`".to_string();
    }
    match retrieve_evidence_steered(
        query,
        search.embedder,
        search.index,
        search.top_k,
        search.steer,
        search.status,
    )
    .await
    {
        Some(block) => block,
        None => "no code matched — the semantic index may be empty or the embedding model \
                 unavailable; use read_file/find if you already know the path"
            .to_string(),
    }
}

/// Human-facing ranked list for `/search` (#1387).
#[must_use]
pub fn format_search_hits(result: &RetrievalResult) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "semantic search — {} candidate(s) → {} shown, {} rejected  [{}] complete={}  {}\n",
        result.candidates,
        result.hits.len(),
        result.rejected.len(),
        result.index_id,
        result.complete,
        EvidenceKind::Semantic.label(),
    ));
    for (i, hit) in result.hits.iter().enumerate() {
        out.push_str(&format!(
            "  {:>2}. {:.3}  {}:{}-{}  {}  cosine={:.3} def={:+.2} path={:+.2}\n",
            i + 1,
            hit.final_score,
            hit.chunk.file,
            hit.chunk.start_line,
            hit.chunk.end_line,
            hit.kind.label(),
            hit.cosine,
            hit.def_boost,
            hit.path_boost,
        ));
    }
    if result.hits.is_empty() {
        out.push_str("  (no hits)\n");
    }
    for w in &result.warnings {
        out.push_str(&format!("  warning: {w}\n"));
    }
    out.push_str(
        "  /search preview N · /search model · /search rejects · /search pin N · /search exclude N · /search status\n",
    );
    out
}

/// Source preview for hit `n` (1-based).
#[must_use]
pub fn format_search_preview(result: &RetrievalResult, n: usize) -> String {
    match result.hits.get(n.saturating_sub(1)) {
        Some(hit) => format!(
            "preview [{n}] {} {}:{}-{}\n{}\n",
            hit.kind.label(),
            hit.chunk.file,
            hit.chunk.start_line,
            hit.chunk.end_line,
            hit.chunk.text
        ),
        None => format!(
            "no hit #{n} — run /search <query> first, or pick 1..{}\n",
            result.hits.len()
        ),
    }
}

/// Reject ledger for `/search rejects`.
#[must_use]
pub fn format_search_rejects(result: &RetrievalResult) -> String {
    let mut out = String::from("reject ledger:\n");
    if result.rejected.is_empty() {
        out.push_str("  (none)\n");
        return out;
    }
    for (hit, reason) in &result.rejected {
        out.push_str(&format!(
            "  {:.3}  {}:{}-{}  {:?}  {}\n",
            hit.final_score,
            hit.chunk.file,
            hit.chunk.start_line,
            hit.chunk.end_line,
            reason,
            hit.kind.label(),
        ));
    }
    out
}

/// Model view — the exact `<code_evidence>` packet that would be injected.
#[must_use]
pub fn format_search_model(result: &RetrievalResult) -> String {
    render_code_evidence(result)
        .unwrap_or_else(|| "(no selected hits — empty model packet)\n".into())
}

/// Index status lines for `/search status`.
#[must_use]
pub fn format_index_status(status: &IndexStatus, steer: &RetrievalSteer) -> String {
    let mut out = String::new();
    out.push_str(&format!("index_id: {}\n", status.index_id()));
    out.push_str(&format!("generation: {}\n", status.generation));
    out.push_str(&format!("complete: {}\n", status.complete()));
    match &status.manifest {
        Some(m) => {
            out.push_str(&format!(
                "gather: {} candidate(s), {} cut(s), hash {}\n",
                m.candidate_count,
                m.cuts.len(),
                m.candidate_hash
            ));
            if !m.cuts.is_empty() {
                for (dir, n) in m.cut_rollup() {
                    out.push_str(&format!("  cut rollup: {dir} × {n}\n"));
                }
            }
        }
        None => out.push_str("gather: (not yet indexed this session)\n"),
    }
    match (&status.git_head, status.dirty) {
        (Some(h), Some(d)) => out.push_str(&format!(
            "git HEAD: {}  dirty: {}\n",
            &h[..h.len().min(12)],
            if d { "yes" } else { "no" }
        )),
        (Some(h), None) => out.push_str(&format!("git HEAD: {}\n", &h[..h.len().min(12)])),
        _ => out.push_str("git: (unavailable)\n"),
    }
    out.push_str(&format!(
        "steering: {} pinned, {} excluded path(s)\n",
        steer.pinned.len(),
        steer.excluded_paths.len()
    ));
    for p in &steer.pinned {
        out.push_str(&format!("  pin {}\n", p.loc_key()));
    }
    for p in &steer.excluded_paths {
        out.push_str(&format!("  exclude {p}\n"));
    }
    out
}

#[cfg(test)]
#[path = "semantic_tests.rs"]
mod tests;
