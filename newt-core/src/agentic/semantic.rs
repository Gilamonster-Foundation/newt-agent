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
/// walk → collect matching candidates with sizes → [`plan_gather`] (sort + cap)
/// → read the kept files, returning `(files, manifest)`. The manifest records
/// the full-walk hash + what the caps dropped.
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
    let root = std::path::Path::new(workspace);
    let mut candidates: Vec<(String, u64)> = Vec::new();
    for entry in ignore::WalkBuilder::new(workspace).build().flatten() {
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str());
        if !ext.is_some_and(|e| extensions.iter().any(|x| x == e)) {
            continue;
        }
        let size = path.metadata().map(|m| m.len()).unwrap_or(u64::MAX);
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        candidates.push((rel, size));
    }
    let (kept, manifest) = plan_gather(&candidates, caps);
    let mut files = Vec::with_capacity(kept.len());
    for rel in &kept {
        if let Ok(src) = std::fs::read_to_string(root.join(rel)) {
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
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    fn cand(paths: &[(&str, u64)]) -> Vec<(String, u64)> {
        paths.iter().map(|(p, s)| (p.to_string(), *s)).collect()
    }

    #[test]
    fn plan_gather_sorts_before_capping() {
        // #1281 / WF-4: the fix. Unstable walk order in, deterministic kept out —
        // the file cap takes the lexicographically-first N, not a random N.
        let caps = GatherCaps {
            max_files: 2,
            max_bytes: 1000,
        };
        let (kept, m) = plan_gather(&cand(&[("z.rs", 1), ("a.rs", 1), ("m.rs", 1)]), caps);
        assert_eq!(kept, vec!["a.rs".to_string(), "m.rs".to_string()]);
        assert_eq!(m.candidate_count, 3);
        // z.rs is cut by the file cap and named honestly.
        assert_eq!(
            m.cuts,
            vec![Cut {
                path: "z.rs".into(),
                class: CutClass::OverFileCap
            }]
        );
    }

    #[test]
    fn plan_gather_is_deterministic_and_double_gather_matches() {
        // The double-gather vector: same tree ⇒ identical manifest (hash + cuts).
        let c = cand(&[("b.rs", 5), ("a.rs", 5)]);
        let (k1, m1) = plan_gather(&c, GatherCaps::default());
        // Re-input in a different order → identical result (sort makes it stable).
        let (k2, m2) = plan_gather(&cand(&[("a.rs", 5), ("b.rs", 5)]), GatherCaps::default());
        assert_eq!(k1, k2);
        assert_eq!(m1, m2);
        assert_eq!(m1.candidate_hash.len(), 64, "blake3 hex");
    }

    #[test]
    fn plan_gather_cuts_oversized_files_without_spending_the_file_budget() {
        // A too-large file is TooLarge (not silently skipped) and does NOT consume
        // the file cap — so a small file after it is still kept.
        let caps = GatherCaps {
            max_files: 1,
            max_bytes: 100,
        };
        let (kept, m) = plan_gather(&cand(&[("big.rs", 500), ("small.rs", 10)]), caps);
        assert_eq!(kept, vec!["small.rs".to_string()]);
        assert_eq!(
            m.cuts,
            vec![Cut {
                path: "big.rs".into(),
                class: CutClass::TooLarge
            }]
        );
    }

    #[test]
    fn cut_rollup_groups_by_top_dir() {
        let caps = GatherCaps {
            max_files: 0,
            max_bytes: 1000,
        };
        let (_, m) = plan_gather(
            &cand(&[("core/a.rs", 1), ("core/b.rs", 1), ("tui/c.rs", 1)]),
            caps,
        );
        assert_eq!(
            m.cut_rollup(),
            vec![("core".to_string(), 2), ("tui".to_string(), 1)]
        );
    }

    #[test]
    fn gather_code_files_honors_the_extension_allowlist_956() {
        // #956: the extension allow-list is a PARAMETER, not a hardcoded rs/py.
        // A bash pack's `.sh` files (and any drop-in pack's) must be gathered when
        // the pack's extension is requested — they were silently dropped before,
        // starving the API-surface block for 4 of 6 built-in languages.
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "newt-gcf-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tool.sh"), "myfunc() { echo hi; }\n").unwrap();
        std::fs::write(dir.join("main.rs"), "pub fn open() {}\n").unwrap();
        let ws = dir.to_string_lossy().to_string();

        // Only `sh` requested → the bash file is gathered; the rs file is not.
        let sh_only = gather_code_files(&ws, &["sh".to_string()]);
        assert!(
            sh_only.iter().any(|(p, _)| p.ends_with("tool.sh")),
            "the .sh file must be gathered when `sh` is requested: {sh_only:?}"
        );
        assert!(
            !sh_only.iter().any(|(p, _)| p.ends_with("main.rs")),
            "rs was not requested: {sh_only:?}"
        );

        // Multiple extensions → the multi-language surface reads both.
        let both = gather_code_files(&ws, &["rs".to_string(), "sh".to_string()]);
        assert!(both.iter().any(|(p, _)| p.ends_with("tool.sh")));
        assert!(both.iter().any(|(p, _)| p.ends_with("main.rs")));

        // Empty allow-list gathers nothing.
        assert!(gather_code_files(&ws, &[]).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn embed_parses_the_vector() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/embeddings"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "embedding": [0.1, 0.2, 0.3] })),
            )
            .mount(&server)
            .await;
        let c = EmbeddingsClient::new(
            server.uri(),
            "nomic-embed-text",
            crate::BackendKind::Ollama,
            None,
            30,
            0,
        );
        assert_eq!(c.embed("hello").await.unwrap(), vec![0.1f32, 0.2, 0.3]);
    }

    #[tokio::test]
    async fn embed_openai_protocol_hits_v1_and_parses_data() {
        // An OpenAI-compatible endpoint (e.g. vLLM serving an embedding model):
        // POST /v1/embeddings with `{input}`, response `{data:[{embedding}]}`.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{ "embedding": [0.5, 0.6] }]
            })))
            .mount(&server)
            .await;
        let c = EmbeddingsClient::new(
            server.uri(),
            "bge-m3",
            crate::BackendKind::Openai,
            None,
            30,
            0,
        );
        assert_eq!(c.embed("hello").await.unwrap(), vec![0.5f32, 0.6]);
        // The request body must use OpenAI's `input` field, not Ollama's
        // `prompt` (guards against a body-shape regression the path match alone
        // wouldn't catch).
        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(body["input"], "hello");
        assert!(body.get("prompt").is_none());
    }

    /// The batch must keep input order. The mock encodes each prompt's length
    /// into the returned vector so the assertion is exact, not incidental.
    #[tokio::test]
    async fn embed_batch_preserves_order() {
        struct ByLen;
        impl Respond for ByLen {
            fn respond(&self, req: &Request) -> ResponseTemplate {
                let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
                let n = body["prompt"].as_str().unwrap_or("").chars().count() as f64;
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "embedding": [n] }))
            }
        }
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/embeddings"))
            .respond_with(ByLen)
            .mount(&server)
            .await;
        let c = EmbeddingsClient::new(server.uri(), "m", crate::BackendKind::Ollama, None, 30, 0);
        let out = c
            .embed_batch(&["a".into(), "bbb".into(), "cc".into()])
            .await
            .unwrap();
        assert_eq!(out, vec![vec![1.0f32], vec![3.0], vec![2.0]]);
    }

    #[tokio::test]
    async fn embed_retries_then_succeeds() {
        struct FailOnce(Arc<AtomicUsize>);
        impl Respond for FailOnce {
            fn respond(&self, _req: &Request) -> ResponseTemplate {
                if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(500)
                } else {
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({ "embedding": [1.0, 2.0] }))
                }
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/embeddings"))
            .respond_with(FailOnce(calls.clone()))
            .mount(&server)
            .await;
        let c = EmbeddingsClient::new(server.uri(), "m", crate::BackendKind::Ollama, None, 30, 2);
        assert_eq!(c.embed("x").await.unwrap(), vec![1.0f32, 2.0]);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "one failure, one success");
    }

    #[tokio::test]
    async fn embed_gives_up_after_retries() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/embeddings"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let c = EmbeddingsClient::new(server.uri(), "m", crate::BackendKind::Ollama, None, 30, 1);
        let err = c.embed("x").await.unwrap_err();
        assert!(err.to_string().contains("returned 500"), "{err}");
    }

    #[tokio::test]
    async fn embed_rejects_missing_and_empty_vector() {
        let server = MockServer::start().await;
        // 200 but no `embedding` key → error, not a silent empty vector.
        Mock::given(method("POST"))
            .and(path("/api/embeddings"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "nope": 1 })),
            )
            .mount(&server)
            .await;
        let c = EmbeddingsClient::new(server.uri(), "m", crate::BackendKind::Ollama, None, 30, 0);
        assert!(c
            .embed("x")
            .await
            .unwrap_err()
            .to_string()
            .contains("missing `embedding`"));
    }

    /// retries=0 (the "disable retries" config boundary) must make EXACTLY one
    /// attempt — pins the `0..=retries` loop bound against an off-by-one mutation.
    #[tokio::test]
    async fn embed_retries_zero_makes_exactly_one_call() {
        struct Count(Arc<AtomicUsize>);
        impl Respond for Count {
            fn respond(&self, _req: &Request) -> ResponseTemplate {
                self.0.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(500)
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/embeddings"))
            .respond_with(Count(calls.clone()))
            .mount(&server)
            .await;
        let c = EmbeddingsClient::new(server.uri(), "m", crate::BackendKind::Ollama, None, 30, 0);
        assert!(c.embed("x").await.is_err());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "retries=0 → exactly one attempt"
        );
    }

    /// The bearer-auth branch must actually emit `Authorization: Bearer <key>`
    /// when an api_key is set (matches the codebase's auth-test convention).
    #[tokio::test]
    async fn embed_sends_bearer_when_api_key_set() {
        use wiremock::matchers::header;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/embeddings"))
            .and(header("authorization", "Bearer sk-test"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "embedding": [1.0] })),
            )
            .mount(&server)
            .await;
        let c = EmbeddingsClient::new(
            server.uri(),
            "m",
            crate::BackendKind::Ollama,
            Some("sk-test".into()),
            30,
            0,
        );
        // The request only matches (and 200s) if the Authorization header is sent.
        assert_eq!(c.embed("x").await.unwrap(), vec![1.0f32]);
    }

    // --- 26.5.2 chunker (pure, &str fixtures — no fs/net) -------------------

    #[test]
    fn chunk_rust_captures_defs_with_leading_doc() {
        let src = "\
//! file header
use std::fmt;

/// Adds two numbers.
fn add(a: i32, b: i32) -> i32 {
    a + b
}

/// A point.
struct Point {
    x: i32,
}
";
        let chunks = chunk_source("src/lib.rs", src);
        assert_eq!(chunks.len(), 2, "one chunk per def: {chunks:#?}");
        // First chunk: the fn, starting at its leading doc line (line 4).
        assert_eq!(chunks[0].kind, "function");
        assert_eq!(chunks[0].start_line, 4);
        assert!(chunks[0].text.contains("/// Adds two numbers."));
        assert!(chunks[0].text.contains("fn add"));
        assert!(
            !chunks[0].text.contains("struct Point"),
            "def boundary respected"
        );
        // Second chunk: the struct, with its doc, to EOF.
        assert_eq!(chunks[1].kind, "struct");
        assert!(chunks[1].text.contains("/// A point.") && chunks[1].text.contains("struct Point"));
    }

    #[test]
    fn chunk_python_def_and_class() {
        let src = "\
import os

def greet(name):
    return f\"hi {name}\"

class Dog:
    def bark(self):
        return \"woof\"
";
        let chunks = chunk_source("app.py", src);
        assert!(chunks
            .iter()
            .any(|c| c.kind == "function" && c.text.contains("def greet")));
        assert!(chunks
            .iter()
            .any(|c| c.kind == "class" && c.text.contains("class Dog")));
    }

    #[test]
    fn chunk_unknown_language_falls_back_to_windows() {
        let src = (1..=90)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = chunk_source("notes.txt", &src); // unknown lang → window fallback
        assert!(chunks.iter().all(|c| c.kind == "window"));
        assert!(
            chunks.len() >= 2,
            "90 lines / 40 ⇒ 3 windows: {}",
            chunks.len()
        );
        // windows are contiguous and cover the whole file
        assert_eq!(chunks.first().unwrap().start_line, 1);
        assert_eq!(chunks.last().unwrap().end_line, 90);
    }

    #[test]
    fn chunk_oversized_body_splits_into_windows() {
        // A fn whose body exceeds CHUNK_MAX_CHARS must split (no monster chunk).
        let body = (0..400)
            .map(|i| format!("    let v{i} = {i};"))
            .collect::<Vec<_>>()
            .join("\n");
        let src = format!("fn big() {{\n{body}\n}}\n");
        let chunks = chunk_source("src/big.rs", &src);
        assert!(chunks.len() > 1, "oversized body split: {}", chunks.len());
        assert!(
            chunks
                .iter()
                .all(|c| c.text.chars().count() <= CHUNK_MAX_CHARS + 200),
            "every chunk stays bounded"
        );
    }

    #[test]
    fn chunk_empty_source_is_empty() {
        assert!(chunk_source("src/lib.rs", "").is_empty());
    }

    // --- 26.5.3 vector store + cosine + retrieval (literal vectors) ---------

    fn chunk(file: &str, text: &str) -> CodeChunk {
        CodeChunk {
            file: file.into(),
            start_line: 1,
            end_line: 1,
            kind: "function".into(),
            text: text.into(),
        }
    }

    #[test]
    fn cosine_known_vectors() {
        assert!(
            (cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6,
            "identical"
        );
        assert!(
            cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6,
            "orthogonal → 0"
        );
        assert!(
            (cosine(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < 1e-6,
            "opposite → -1"
        );
        // defensive: dim mismatch / empty / zero → 0, never NaN or panic
        assert_eq!(cosine(&[1.0, 2.0, 3.0], &[1.0, 2.0]), 0.0);
        assert_eq!(cosine(&[], &[]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn index_search_orders_by_cosine_and_truncates() {
        let idx = SessionSemanticIndex::default();
        idx.index_chunk(chunk("a.rs", "alpha"), vec![1.0, 0.0]);
        idx.index_chunk(chunk("b.rs", "beta"), vec![0.0, 1.0]);
        idx.index_chunk(chunk("c.rs", "gamma"), vec![0.9, 0.1]);
        assert_eq!(idx.chunks_indexed(), 3);
        assert_eq!(idx.indexed_chars(), 5 + 4 + 5);
        // query aligned with x-axis → a.rs (1,0) best, then c.rs (0.9,0.1), then b.rs
        let hits = idx.search(&[1.0, 0.0], 2);
        assert_eq!(hits.len(), 2, "top_k truncates");
        assert_eq!(hits[0].1.file, "a.rs");
        assert_eq!(hits[1].1.file, "c.rs");
        assert!(hits[0].0 > hits[1].0, "descending score");
        // top_k larger than the index → all; empty query vec → all score 0
        assert_eq!(idx.search(&[1.0, 0.0], 99).len(), 3);
        // empty index → no hits
        let empty = SessionSemanticIndex::default();
        assert!(empty.search(&[1.0, 0.0], 5).is_empty());
        // clear empties it
        idx.clear();
        assert_eq!(idx.chunks_indexed(), 0);
    }

    // --- 26.5.4 index_files + retrieve_evidence (mock Embedder, no fs/net) --

    /// Deterministic fake: embed text → [count('a'), count('b'), len]. Lets the
    /// retrieval assertions be exact without any network.
    struct MockEmbedder;
    #[async_trait]
    impl Embedder for MockEmbedder {
        async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
            Ok(vec![
                text.matches('a').count() as f32,
                text.matches('b').count() as f32,
                text.chars().count() as f32,
            ])
        }
    }

    /// An embedder that always fails — stands in for an unpulled model.
    struct FailEmbedder;
    #[async_trait]
    impl Embedder for FailEmbedder {
        async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            anyhow::bail!("embeddings model not available")
        }
    }

    /// Always-failing embedder that counts attempts — distinguishes the
    /// `Disable` (stop after one) and `Warn` (try every chunk) policies.
    struct CountingFailEmbedder(Arc<AtomicUsize>);
    #[async_trait]
    impl Embedder for CountingFailEmbedder {
        async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("embeddings endpoint returned 404")
        }
    }

    #[tokio::test]
    async fn index_files_disable_stops_on_first_failure() {
        // Two chunks; a structural failure under Disable must stop after the
        // FIRST embed attempt (not spam one per chunk) and index nothing.
        let files = vec![("a.rs".to_string(), "fn add() {}\nfn sub() {}".to_string())];
        let idx = SessionSemanticIndex::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let n = index_files(
            &files,
            &CountingFailEmbedder(calls.clone()),
            &idx,
            crate::OnEmbedFailure::Disable,
        )
        .await;
        assert_eq!(n, 0);
        assert_eq!(idx.chunks_indexed(), 0);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "Disable must short-circuit after the first failure"
        );

        // Warn, by contrast, attempts every chunk (>= 2 here).
        let calls2 = Arc::new(AtomicUsize::new(0));
        index_files(
            &files,
            &CountingFailEmbedder(calls2.clone()),
            &SessionSemanticIndex::default(),
            crate::OnEmbedFailure::Warn,
        )
        .await;
        assert!(
            calls2.load(Ordering::SeqCst) >= 2,
            "Warn keeps trying per-chunk, got {}",
            calls2.load(Ordering::SeqCst)
        );
    }

    #[tokio::test]
    async fn index_files_chunks_embeds_and_skips_failures() {
        let files = vec![("a.rs".to_string(), "fn add() {}\nfn sub() {}".to_string())];
        // happy path: two fns chunked + embedded
        let idx = SessionSemanticIndex::default();
        let n = index_files(&files, &MockEmbedder, &idx, crate::OnEmbedFailure::Disable).await;
        assert_eq!(n, idx.chunks_indexed() as usize);
        assert!(n >= 2, "two fns indexed, got {n}");
        // Warn policy: all embeds fail → nothing indexed, no panic (it keeps
        // going per-chunk, the historical degrade).
        let empty = SessionSemanticIndex::default();
        assert_eq!(
            index_files(&files, &FailEmbedder, &empty, crate::OnEmbedFailure::Warn).await,
            0
        );
        assert_eq!(empty.chunks_indexed(), 0);
    }

    #[tokio::test]
    async fn retrieve_evidence_embeds_query_ranks_and_degrades() {
        let files = vec![("a.rs".to_string(), "fn aaa() {}\nfn bbb() {}".to_string())];
        let idx = SessionSemanticIndex::default();
        index_files(&files, &MockEmbedder, &idx, crate::OnEmbedFailure::Disable).await;
        // query rich in 'a' → the aaa chunk outranks bbb (cosine on the a-axis)
        let block = retrieve_evidence("aaaaa", &MockEmbedder, &idx, 1)
            .await
            .unwrap();
        assert!(
            block.contains("<code_evidence>") && block.contains("aaa"),
            "{block}"
        );
        // a failed query embed → None (absent model = silent no-op, not a crash)
        assert!(retrieve_evidence("x", &FailEmbedder, &idx, 1)
            .await
            .is_none());
        // empty index → None
        let empty = SessionSemanticIndex::default();
        assert!(retrieve_evidence("aaa", &MockEmbedder, &empty, 1)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn code_search_tool_embeds_searches_and_coaches() {
        let files = vec![("a.rs".to_string(), "fn aaa() {}\nfn bbb() {}".to_string())];
        let idx = SessionSemanticIndex::default();
        index_files(&files, &MockEmbedder, &idx, crate::OnEmbedFailure::Disable).await;
        let search = CodeSearch {
            embedder: &MockEmbedder,
            index: &idx,
            top_k: 1,
            steer: None,
            status: None,
        };
        // a query → the matching <code_evidence>
        let out =
            execute_code_search(&serde_json::json!({"query": "aaaaa"}), search, false, 20).await;
        assert!(
            out.contains("<code_evidence>") && out.contains("aaa"),
            "{out}"
        );
        // empty query → coaching, not a search
        assert!(
            execute_code_search(&serde_json::json!({}), search, false, 20)
                .await
                .starts_with("error:")
        );
        // empty index → a labelled no-match (never an empty string)
        let empty = SessionSemanticIndex::default();
        let s2 = CodeSearch {
            embedder: &MockEmbedder,
            index: &empty,
            top_k: 1,
            steer: None,
            status: None,
        };
        assert!(
            execute_code_search(&serde_json::json!({"query": "x"}), s2, false, 20)
                .await
                .contains("no code matched")
        );
    }

    #[test]
    fn code_search_tool_definition_shape() {
        let def = code_search_tool_definition();
        assert_eq!(def["function"]["name"], "code_search");
        assert!(def["function"]["parameters"]["properties"]["query"].is_object());
    }

    #[test]
    fn rerank_boosts_defs_and_paths_and_stays_stable() {
        let c = |file: &str, kind: &str| CodeChunk {
            file: file.to_string(),
            start_line: 1,
            end_line: 2,
            kind: kind.to_string(),
            text: "x".to_string(),
        };
        // a real def beats a raw window at EQUAL cosine
        let mut hits = vec![(0.5, c("a.rs", "window")), (0.5, c("b.rs", "function"))];
        rerank("anything", &mut hits);
        assert_eq!(hits[0].1.kind, "function", "def promoted over window");
        // a file-path term match (+0.05) overtakes a 0.02 cosine gap
        let mut hits = vec![
            (0.50, c("other.rs", "window")),
            (0.48, c("retry.rs", "window")),
        ];
        rerank("where is retry handled", &mut hits);
        assert_eq!(hits[0].1.file, "retry.rs", "path-term match promoted");
        // a LARGE cosine gap is NOT overridden by the small boost
        let mut hits = vec![(0.90, c("x.rs", "window")), (0.50, c("y.rs", "function"))];
        rerank("anything", &mut hits);
        assert_eq!(
            hits[0].1.file, "x.rs",
            "strong cosine wins over a small boost"
        );
        // no applicable boost → a cosine-sorted input is preserved bit-for-bit
        let mut hits = vec![
            (0.9, c("a.rs", "window")),
            (0.6, c("b.rs", "window")),
            (0.4, c("c.rs", "window")),
        ];
        let before = hits.clone();
        rerank("zz", &mut hits); // "zz" < 3 chars → no terms → no boost
        assert_eq!(hits, before, "no boost → cosine order unchanged");
        // stable on ties: equal final score keeps input order
        let mut hits = vec![
            (0.5, c("first.rs", "window")),
            (0.5, c("second.rs", "window")),
        ];
        rerank("zz", &mut hits);
        assert_eq!(hits[0].1.file, "first.rs", "stable: ties keep input order");
    }

    // --- #1387 Phase 1: structured RetrievalResult --------------------------

    fn hit(file: &str, kind: &str, cosine: f32) -> RankedHit {
        let chunk = CodeChunk {
            file: file.to_string(),
            start_line: 1,
            end_line: 2,
            kind: kind.to_string(),
            text: format!("body of {file}"),
        };
        let (def_boost, path_boost) = boosts_for(&[], &chunk);
        RankedHit {
            chunk,
            kind: EvidenceKind::Semantic,
            cosine,
            def_boost,
            path_boost,
            final_score: cosine + def_boost + path_boost,
        }
    }

    #[test]
    fn ranked_hits_decompose_cosine_and_boosts() {
        let ranked = rank_hits(
            "retry backoff",
            vec![
                (
                    0.50,
                    CodeChunk {
                        file: "other.rs".into(),
                        start_line: 1,
                        end_line: 1,
                        kind: "window".into(),
                        text: "x".into(),
                    },
                ),
                (
                    0.48,
                    CodeChunk {
                        file: "retry.rs".into(),
                        start_line: 1,
                        end_line: 1,
                        kind: "function".into(),
                        text: "fn retry() {}".into(),
                    },
                ),
            ],
        );
        assert_eq!(ranked[0].chunk.file, "retry.rs");
        assert!(ranked[0].def_boost > 0.0, "def boost applied");
        assert!(ranked[0].path_boost > 0.0, "path boost applied");
        assert!(
            (ranked[0].final_score
                - (ranked[0].cosine + ranked[0].def_boost + ranked[0].path_boost))
                .abs()
                < 1e-6
        );
        assert_eq!(ranked[0].kind, EvidenceKind::Semantic);
    }

    #[test]
    fn apply_budget_rejects_overflow_as_budget_exhausted() {
        let selected = vec![hit("a.rs", "function", 0.9), hit("b.rs", "function", 0.8)];
        let mut rejected = Vec::new();
        // Tiny cap: only the wrapper fits → both hits budget-rejected.
        let kept = apply_budget(selected, &mut rejected, 40);
        assert!(kept.is_empty());
        assert_eq!(rejected.len(), 2);
        assert!(rejected
            .iter()
            .all(|(_, r)| *r == RejectReason::BudgetExhausted));
    }

    #[test]
    fn index_status_complete_follows_gather_cuts() {
        let mut status = IndexStatus {
            generation: 2,
            manifest: Some(GatherManifest {
                candidate_count: 3,
                candidate_hash: "abcd1234ffff".into(),
                max_files: 2,
                max_bytes: 100,
                cuts: vec![Cut {
                    path: "z.rs".into(),
                    class: CutClass::OverFileCap,
                }],
            }),
            git_head: Some("deadbeefcafe".into()),
            dirty: Some(true),
        };
        assert!(!status.complete());
        assert_eq!(status.index_id(), "gen2:abcd1234");
        status.manifest.as_mut().unwrap().cuts.clear();
        assert!(status.complete());
    }

    #[test]
    fn pin_exclude_filters_shape_retrieval_result() {
        let mut steer = RetrievalSteer::default();
        let pinned = hit("keep.rs", "function", 0.1);
        steer.pin(pinned.clone());
        steer.exclude_path("skip.rs");
        assert!(steer.is_excluded("skip.rs"));
        assert!(steer.is_excluded("skip.rs/nested.rs"));
        assert!(!steer.is_excluded("keep.rs"));

        let mut rejected = Vec::new();
        let eligible = vec![
            hit("skip.rs", "function", 0.99),
            hit("keep.rs", "function", 0.50),
            hit("other.rs", "window", 0.40),
        ];
        // Simulate the filter + top_k=1 + pin union used by retrieve_ranked.
        let mut selected = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for h in eligible {
            if steer.is_excluded(&h.chunk.file) {
                rejected.push((h, RejectReason::Excluded));
            } else if selected.is_empty() {
                seen.insert(h.loc_key());
                selected.push(h);
            } else {
                rejected.push((h, RejectReason::BelowTopK));
            }
        }
        for pin in steer.pinned.iter().rev() {
            let key = pin.loc_key();
            if seen.insert(key.clone()) {
                rejected.retain(|(h, _)| h.loc_key() != key);
                selected.insert(0, pin.clone());
            }
        }
        assert!(rejected
            .iter()
            .any(|(h, r)| h.chunk.file == "skip.rs" && *r == RejectReason::Excluded));
        assert!(selected.iter().any(|h| h.chunk.file == "keep.rs"));
    }

    #[test]
    fn render_code_evidence_compatible_with_legacy_packet_shape() {
        let result = RetrievalResult {
            hits: vec![hit("src/lib.rs", "function", 0.9)],
            rejected: vec![],
            candidates: 1,
            complete: true,
            index_id: "gen1:abcd1234".into(),
            warnings: vec![],
        };
        let block = render_code_evidence(&result).unwrap();
        assert!(block.starts_with("<code_evidence>\n"));
        assert!(block.ends_with("</code_evidence>"));
        assert!(block.contains("src/lib.rs:1-2"), "{block}");
        assert!(block.contains("[SEMANTIC]"), "{block}");
        assert!(
            block.contains("not proof"),
            "semantic honesty note: {block}"
        );
        assert!(render_code_evidence(&RetrievalResult {
            hits: vec![],
            rejected: vec![],
            candidates: 0,
            complete: true,
            index_id: "gen0".into(),
            warnings: vec![],
        })
        .is_none());
    }

    #[tokio::test]
    async fn retrieve_ranked_reports_completeness_and_exclusions() {
        let files = vec![
            ("keep.rs".to_string(), "fn keep_me() {}".to_string()),
            ("skip.rs".to_string(), "fn skip_me() {}".to_string()),
        ];
        let idx = SessionSemanticIndex::default();
        index_files(&files, &MockEmbedder, &idx, crate::OnEmbedFailure::Disable).await;
        let status = IndexStatus {
            generation: 1,
            manifest: Some(GatherManifest {
                candidate_count: 2,
                candidate_hash: "ffffffff".into(),
                max_files: 400,
                max_bytes: 200_000,
                cuts: vec![],
            }),
            git_head: None,
            dirty: None,
        };
        let mut steer = RetrievalSteer::default();
        steer.exclude_path("skip.rs");
        let result = retrieve_ranked("keep", &MockEmbedder, &idx, 5, Some(&steer), Some(&status))
            .await
            .unwrap();
        assert!(result.complete);
        assert_eq!(result.index_id, "gen1:ffffffff");
        assert!(result
            .rejected
            .iter()
            .any(|(h, r)| h.chunk.file.contains("skip") && *r == RejectReason::Excluded));
        assert!(result.hits.iter().all(|h| !h.chunk.file.contains("skip")));
        let rendered = render_code_evidence(&result).unwrap();
        assert!(rendered.contains("<code_evidence>"));
    }

    fn sample_chunk(file: &str, kind: &str, text: &str) -> CodeChunk {
        CodeChunk {
            file: file.to_string(),
            start_line: 1,
            end_line: 2,
            kind: kind.to_string(),
            text: text.to_string(),
        }
    }

    #[tokio::test]
    async fn retrieve_ranked_decomposes_scores_and_rejects() {
        let files = vec![
            (
                "retry.rs".to_string(),
                "fn retry_backoff() { /* aaaaa */ }\n".to_string(),
            ),
            ("other.rs".to_string(), "fn unrelated() {}\n".to_string()),
        ];
        let idx = SessionSemanticIndex::default();
        index_files(&files, &MockEmbedder, &idx, crate::OnEmbedFailure::Disable).await;
        let status = IndexStatus {
            generation: 2,
            manifest: Some(GatherManifest {
                candidate_count: 2,
                candidate_hash: "abcdef0123456789".into(),
                max_files: 400,
                max_bytes: 200_000,
                cuts: vec![],
            }),
            git_head: None,
            dirty: None,
        };
        let result = retrieve_ranked("aaaaa retry", &MockEmbedder, &idx, 1, None, Some(&status))
            .await
            .expect("hits");
        assert!(result.complete);
        assert!(result.index_id.starts_with("gen2:"));
        assert!(!result.hits.is_empty());
        assert_eq!(result.hits[0].kind, EvidenceKind::Semantic);
        assert!(result.hits[0].final_score >= result.hits[0].cosine);
        // BelowTopK rejects when over-fetched
        assert!(
            result
                .rejected
                .iter()
                .any(|(_, r)| *r == RejectReason::BelowTopK)
                || result.candidates <= 1,
            "expected BelowTopK or single candidate: {:?}",
            result.rejected
        );
        let rendered = render_code_evidence(&result).unwrap();
        assert!(rendered.contains("[SEMANTIC]"));
        assert!(rendered.contains("<code_evidence>"));
    }

    #[tokio::test]
    async fn retrieve_ranked_pin_exclude_and_budget() {
        let files = vec![
            ("keep/a.rs".to_string(), "fn aaa() {}\n".to_string()),
            ("skip/b.rs".to_string(), "fn aaa_bbb() {}\n".to_string()),
        ];
        let idx = SessionSemanticIndex::default();
        index_files(&files, &MockEmbedder, &idx, crate::OnEmbedFailure::Disable).await;
        let mut steer = RetrievalSteer::default();
        steer.exclude_path("skip");
        let status = IndexStatus {
            generation: 1,
            manifest: Some(GatherManifest {
                candidate_count: 99,
                candidate_hash: "deadbeef".into(),
                max_files: 400,
                max_bytes: 200_000,
                cuts: vec![Cut {
                    path: "big.rs".into(),
                    class: CutClass::TooLarge,
                }],
            }),
            ..Default::default()
        };
        assert!(!status.complete());
        let result = retrieve_ranked_with_cap(
            "aaa",
            &MockEmbedder,
            &idx,
            5,
            80, // tiny budget → BudgetExhausted
            Some(&steer),
            Some(&status),
        )
        .await
        .expect("some result");
        assert!(!result.complete);
        assert!(
            result
                .rejected
                .iter()
                .any(|(_, r)| *r == RejectReason::Excluded)
                || result
                    .hits
                    .iter()
                    .all(|h| !h.chunk.file.starts_with("skip")),
            "exclude should filter skip/: {result:?}"
        );
        // Pin a hit and ensure it appears
        if let Some(hit) = result.hits.first().cloned() {
            steer.pin(hit.clone());
            let pinned =
                retrieve_ranked("aaa", &MockEmbedder, &idx, 1, Some(&steer), Some(&status))
                    .await
                    .unwrap();
            assert!(pinned.hits.iter().any(|h| h.loc_key() == hit.loc_key()));
        }
    }

    #[test]
    fn format_search_surfaces_include_kind_labels() {
        let result = RetrievalResult {
            hits: vec![RankedHit {
                chunk: sample_chunk("a.rs", "function", "fn a() {}"),
                kind: EvidenceKind::Semantic,
                cosine: 0.5,
                def_boost: 0.05,
                path_boost: 0.0,
                final_score: 0.55,
            }],
            rejected: vec![(
                RankedHit {
                    chunk: sample_chunk("b.rs", "window", "x"),
                    kind: EvidenceKind::Semantic,
                    cosine: 0.1,
                    def_boost: 0.0,
                    path_boost: 0.0,
                    final_score: 0.1,
                },
                RejectReason::BelowTopK,
            )],
            candidates: 2,
            complete: false,
            index_id: "gen1:abcd".into(),
            warnings: vec!["incomplete".into()],
        };
        let hits = format_search_hits(&result);
        assert!(hits.contains("[SEMANTIC]"));
        assert!(hits.contains("complete=false"));
        assert!(format_search_rejects(&result).contains("BelowTopK"));
        assert!(format_search_preview(&result, 1).contains("fn a()"));
        assert!(format_search_model(&result).contains("<code_evidence>"));
        let status = IndexStatus {
            generation: 1,
            manifest: None,
            ..Default::default()
        };
        assert!(format_index_status(&status, &RetrievalSteer::default()).contains("index_id:"));
    }
}
