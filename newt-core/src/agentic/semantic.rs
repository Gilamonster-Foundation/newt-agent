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

use super::display::{print_tool_call, print_tool_output};
use async_trait::async_trait;
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

/// The real [`Embedder`]: Ollama `POST /api/embeddings` (`{model, prompt}` →
/// `{embedding: [f32]}`). Mirrors the summarizer's HTTP discipline — a
/// configurable timeout, optional bearer auth, and exponential-backoff retry
/// (embedding a whole repo is many requests; transient failures recover).
pub struct EmbeddingsClient {
    url: String,
    model: String,
    api_key: Option<String>,
    timeout_secs: u64,
    retries: u32,
}

impl EmbeddingsClient {
    pub fn new(
        url: impl Into<String>,
        model: impl Into<String>,
        api_key: Option<String>,
        timeout_secs: u64,
        retries: u32,
    ) -> Self {
        Self {
            url: url.into(),
            model: model.into(),
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

    /// One embeddings request (no retry — the retry loop wraps this).
    async fn embed_once(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let endpoint = format!("{}/api/embeddings", self.url.trim_end_matches('/'));
        let body = serde_json::json!({ "model": self.model, "prompt": text });
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(self.timeout_secs))
            .build()?;
        let mut req = client.post(&endpoint).json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("embeddings endpoint {}", resp.status());
        }
        let json: serde_json::Value = resp.json().await?;
        let arr = json["embedding"]
            .as_array()
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

/// Render a `<code_evidence>` block from already-ranked `hits` (Step 26.5.3).
/// `None` when there are no hits — the OFF/empty bit-for-bit guarantee (mirror
/// `scratchpad::build_state_block`). Hard-capped at `total_cap` chars so
/// retrieval can't blow the send budget.
fn render_hits(hits: &[(f32, CodeChunk)], total_cap: usize) -> Option<String> {
    if hits.is_empty() {
        return None;
    }
    let mut body = String::from("<code_evidence>\n");
    for (score, chunk) in hits {
        let piece = format!(
            "// {}:{}-{} ({}, score {score:.2})\n{}\n\n",
            chunk.file, chunk.start_line, chunk.end_line, chunk.kind, chunk.text
        );
        if body.chars().count() + piece.chars().count() + "</code_evidence>".len() > total_cap {
            body.push_str("[… more evidence omitted to fit the budget …]\n");
            break;
        }
        body.push_str(&piece);
    }
    body.push_str("</code_evidence>");
    Some(body)
}

/// Render the `<code_evidence>` block for a query VECTOR (Step 26.5.3): search
/// by cosine, render the top_k. The raw-cosine path (no rerank) behind the
/// vector-only `code_evidence_block` entry — `retrieve_evidence` is the reranked
/// path. `None` when the index has no hits.
pub(crate) fn build_code_evidence_block(
    index: &dyn SemanticIndex,
    query: &[f32],
    top_k: usize,
    total_cap: usize,
) -> Option<String> {
    render_hits(&index.search(query, top_k), total_cap)
}

/// Render the `<code_evidence>` block with the default budget cap (Step 26.5) —
/// the TUI-facing entry called per turn. `None` when retrieval finds nothing.
pub fn code_evidence_block(
    index: &dyn SemanticIndex,
    query: &[f32],
    top_k: usize,
) -> Option<String> {
    build_code_evidence_block(index, query, top_k, CODE_EVIDENCE_CAP)
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
) -> usize {
    let mut indexed = 0;
    for (file, source) in files {
        for chunk in chunk_source(file, source) {
            match embedder.embed(&chunk.text).await {
                Ok(v) => {
                    index.index_chunk(chunk, v);
                    indexed += 1;
                }
                Err(e) => {
                    tracing::warn!(error = %e, file = file.as_str(), "embed failed; skipping chunk");
                }
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

/// Re-score cosine `hits` with cheap, deterministic boosts (Step 26.5.6): a
/// definition beats a raw window, and a file path matching a query term gets a
/// nudge — then a STABLE re-sort by `(cosine + boost)` descending. The boosts
/// are small, so they only reorder near-ties; with no boost applicable the
/// cosine order is preserved bit-for-bit (a stable sort on already-cosine-sorted
/// input). Pure + deterministic — no clock, no allocation beyond the term split.
fn rerank(query: &str, hits: &mut [(f32, CodeChunk)]) {
    let terms: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 3)
        .map(str::to_lowercase)
        .collect();
    let boost = |chunk: &CodeChunk| -> f32 {
        let mut b = 0.0;
        if chunk.kind != "window" {
            b += DEF_BOOST;
        }
        let file_lc = chunk.file.to_lowercase();
        if terms.iter().any(|t| file_lc.contains(t.as_str())) {
            b += PATH_BOOST;
        }
        b
    };
    hits.sort_by(|a, b| {
        let sb = b.0 + boost(&b.1);
        let sa = a.0 + boost(&a.1);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });
}

/// Retrieve a reranked `<code_evidence>` block for `query` (Step 26.5.4 +
/// 26.5.6): embed the query, over-fetch cosine candidates, rerank with the cheap
/// structural boosts, take the top_k, render. `None` when the query can't embed,
/// the index is empty, or nothing matches — so an absent embedding model is a
/// silent no-op, not a turn failure.
pub async fn retrieve_evidence(
    query: &str,
    embedder: &dyn Embedder,
    index: &dyn SemanticIndex,
    top_k: usize,
) -> Option<String> {
    let qv = embedder.embed(query).await.ok()?;
    let mut hits = index.search(&qv, top_k.saturating_mul(RERANK_OVERFETCH));
    rerank(query, &mut hits);
    hits.truncate(top_k);
    render_hits(&hits, CODE_EVIDENCE_CAP)
}

/// Walk `workspace` for indexable code files (Step 26.5.4) — gitignore-aware,
/// `.rs`/`.py` only (what the chunker understands), bounded for responsiveness.
/// Returns `(relative-path, source)` pairs. **Runtime fs glue** (NOT unit-tier:
/// it reads the real filesystem); the pure chunk/embed/index logic it feeds is
/// the fully-mocked part above. Reuses the `ignore` crate (newt-core's `find`
/// tool already depends on it).
pub fn gather_code_files(workspace: &str) -> Vec<(String, String)> {
    const MAX_FILES: usize = 400;
    const MAX_BYTES: u64 = 200_000;
    let mut out = Vec::new();
    for entry in ignore::WalkBuilder::new(workspace).build().flatten() {
        if out.len() >= MAX_FILES {
            break;
        }
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str());
        if !matches!(ext, Some("rs") | Some("py")) {
            continue;
        }
        if path.metadata().map(|m| m.len()).unwrap_or(u64::MAX) > MAX_BYTES {
            continue;
        }
        if let Ok(src) = std::fs::read_to_string(path) {
            let rel = path
                .strip_prefix(workspace)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();
            out.push((rel, src));
        }
    }
    out
}

// --- Step 26.5.5: the code_search tool (model-callable retrieval) -----------

/// The semantic searcher handed to the `code_search` tool (Step 26.5.5): an
/// embedder + the session index + the default top_k, bundled into ONE `ChatCtx`
/// field (both members are shared refs, so this is `Copy`).
#[derive(Clone, Copy)]
pub struct CodeSearch<'a> {
    pub embedder: &'a dyn Embedder,
    pub index: &'a dyn SemanticIndex,
    pub top_k: usize,
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
                            code chunks with their file:line; then read_file the ones you need.",
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

/// Execute a `code_search` call (Step 26.5.5): embed the query, search the
/// index, return the matching `<code_evidence>` (or a labelled no-match).
pub(crate) async fn execute_code_search(
    args: &serde_json::Value,
    search: CodeSearch<'_>,
    color: bool,
    tool_output_lines: usize,
) -> String {
    let query = args["query"].as_str().unwrap_or("").trim();
    print_tool_call("code_search", query, color);
    if query.is_empty() {
        return "error: code_search requires a non-empty `query`".to_string();
    }
    let out = match retrieve_evidence(query, search.embedder, search.index, search.top_k).await {
        Some(block) => block,
        None => "no code matched — the semantic index may be empty or the embedding model \
                 unavailable; use read_file/find if you already know the path"
            .to_string(),
    };
    print_tool_output(&out, tool_output_lines, color);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

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
        let c = EmbeddingsClient::new(server.uri(), "nomic-embed-text", None, 30, 0);
        assert_eq!(c.embed("hello").await.unwrap(), vec![0.1f32, 0.2, 0.3]);
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
        let c = EmbeddingsClient::new(server.uri(), "m", None, 30, 0);
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
        let c = EmbeddingsClient::new(server.uri(), "m", None, 30, 2);
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
        let c = EmbeddingsClient::new(server.uri(), "m", None, 30, 1);
        let err = c.embed("x").await.unwrap_err();
        assert!(err.to_string().contains("embeddings endpoint 500"), "{err}");
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
        let c = EmbeddingsClient::new(server.uri(), "m", None, 30, 0);
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
        let c = EmbeddingsClient::new(server.uri(), "m", None, 30, 0);
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
        let c = EmbeddingsClient::new(server.uri(), "m", Some("sk-test".into()), 30, 0);
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

    #[test]
    fn code_evidence_block_none_when_empty_renders_and_caps() {
        let idx = SessionSemanticIndex::default();
        // empty index → None (OFF/empty bit-for-bit)
        assert_eq!(build_code_evidence_block(&idx, &[1.0, 0.0], 5, 6_000), None);
        idx.index_chunk(chunk("src/lib.rs", "fn add() {}"), vec![1.0, 0.0]);
        let block = build_code_evidence_block(&idx, &[1.0, 0.0], 5, 6_000).unwrap();
        assert!(block.starts_with("<code_evidence>\n") && block.ends_with("</code_evidence>"));
        assert!(
            block.contains("src/lib.rs:1-1"),
            "file:line header: {block}"
        );
        assert!(block.contains("fn add() {}"));
        // total cap truncates: tiny cap → the omitted marker, bounded length
        let capped = build_code_evidence_block(&idx, &[1.0, 0.0], 5, 30).unwrap();
        assert!(capped.contains("omitted to fit"), "{capped}");
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

    #[tokio::test]
    async fn index_files_chunks_embeds_and_skips_failures() {
        let files = vec![("a.rs".to_string(), "fn add() {}\nfn sub() {}".to_string())];
        // happy path: two fns chunked + embedded
        let idx = SessionSemanticIndex::default();
        let n = index_files(&files, &MockEmbedder, &idx).await;
        assert_eq!(n, idx.chunks_indexed() as usize);
        assert!(n >= 2, "two fns indexed, got {n}");
        // all embeds fail → nothing indexed, no panic (graceful degrade)
        let empty = SessionSemanticIndex::default();
        assert_eq!(index_files(&files, &FailEmbedder, &empty).await, 0);
        assert_eq!(empty.chunks_indexed(), 0);
    }

    #[tokio::test]
    async fn retrieve_evidence_embeds_query_ranks_and_degrades() {
        let files = vec![("a.rs".to_string(), "fn aaa() {}\nfn bbb() {}".to_string())];
        let idx = SessionSemanticIndex::default();
        index_files(&files, &MockEmbedder, &idx).await;
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
        index_files(&files, &MockEmbedder, &idx).await;
        let search = CodeSearch {
            embedder: &MockEmbedder,
            index: &idx,
            top_k: 1,
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
}
