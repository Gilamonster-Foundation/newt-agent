use super::*;

// Semantic retrieval defaults and failure policy.

#[test]
fn semantic_config_defaults_and_parses() {
    // Defaults (Step 26.5.4): nomic-embed-text, top_k 5, no decoupled
    // endpoint, and on_embed_failure = disable (the safe default).
    let d = SemanticConfig::default();
    assert_eq!(d.embedding_model, "nomic-embed-text");
    assert_eq!(d.top_k, 5);
    assert_eq!(d.embeddings_endpoint, None);
    assert_eq!(d.embeddings_api, None);
    assert_eq!(d.on_embed_failure, OnEmbedFailure::Disable);
    // #720: the embedded-embedder local model dir defaults to None.
    assert_eq!(d.embedding_model_path, None);
    // `[context.semantic]` parses + overrides, incl. the new fields.
    let c: ContextConfig = toml::from_str(
        "[semantic]\nembedding_model = \"mxbai-embed-large\"\ntop_k = 8\n\
             embedding_model_path = \"/models/bge-small-en-v1.5\"\n\
             embeddings_endpoint = \"http://REDACTED-HOST:11434\"\n\
             embeddings_api = \"ollama\"\non_embed_failure = \"warn\"",
    )
    .unwrap();
    assert_eq!(c.semantic.embedding_model, "mxbai-embed-large");
    assert_eq!(
        c.semantic.embedding_model_path.as_deref(),
        Some("/models/bge-small-en-v1.5")
    );
    assert_eq!(c.semantic.top_k, 8);
    assert_eq!(
        c.semantic.embeddings_endpoint.as_deref(),
        Some("http://REDACTED-HOST:11434")
    );
    assert_eq!(c.semantic.embeddings_api, Some(BackendKind::Ollama));
    assert_eq!(c.semantic.on_embed_failure, OnEmbedFailure::Warn);
    // `embeddings_api = "vllm"` aliases to the OpenAI protocol.
    let v: ContextConfig = toml::from_str("[semantic]\nembeddings_api = \"vllm\"").unwrap();
    assert_eq!(v.semantic.embeddings_api, Some(BackendKind::Openai));
    // an absent [context.semantic] still yields the defaults
    let bare: ContextConfig = toml::from_str("manager = \"standard\"").unwrap();
    assert_eq!(bare.semantic, SemanticConfig::default());
}
