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
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "nope": 1 })))
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
    let out = execute_code_search(&serde_json::json!({"query": "aaaaa"}), search, false, 20).await;
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
        (ranked[0].final_score - (ranked[0].cosine + ranked[0].def_boost + ranked[0].path_boost))
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
        let pinned = retrieve_ranked("aaa", &MockEmbedder, &idx, 1, Some(&steer), Some(&status))
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
